/*
 * Hurl (https://hurl.dev)
 * Copyright (C) 2023 Orange
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *          http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 */
mod cli;

use std::env;
use std::io::prelude::*;
use std::path::Path;
use std::time::Instant;

use atty::Stream;
use clap::Command;
use colored::*;

use hurl::report;
use hurl::report::html;
use hurl::runner;
use hurl::runner::HurlResult;
use hurl::util::logger::{BaseLogger, Logger, LoggerBuilder};
use hurl::{http, output};
use hurl_core::ast::HurlFile;
use hurl_core::parser;
use report::junit;

const EXIT_OK: i32 = 0;
const EXIT_ERROR_COMMANDLINE: i32 = 1;
const EXIT_ERROR_PARSING: i32 = 2;
const EXIT_ERROR_RUNTIME: i32 = 3;
const EXIT_ERROR_ASSERT: i32 = 4;
const EXIT_ERROR_UNDEFINED: i32 = 127;

/// Structure that stores the result of an Hurl file execution, and the content of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HurlRun {
    /// Source string for this [`HurlFile`]
    pub content: String,
    /// Filename of the content
    pub filename: String,
    pub hurl_file: HurlFile,
    pub hurl_result: HurlResult,
}

/// Executes Hurl entry point.
fn main() {
    init_colored();

    let libcurl_version = http::libcurl_version_info();
    let version_info = format!(
        "{} {}\nFeatures (libcurl):  {}\nFeatures (built-in): brotli",
        clap::crate_version!(),
        libcurl_version.libraries.join(" "),
        libcurl_version.features.join(" "),
    );
    let mut app = cli::app(&version_info);
    let matches = app.clone().get_matches();

    // We create a basic logger that can just display info, warning or error generic messages.
    // We'll use a more advanced logger for rich error report when running Hurl files.
    let verbose = cli::has_flag(&matches, "verbose")
        || cli::has_flag(&matches, "very_verbose")
        || cli::has_flag(&matches, "interactive");
    let color = cli::output_color(&matches);
    let base_logger = BaseLogger::new(color, verbose);

    let cli_options = cli::parse_options(&matches);
    let cli_options = unwrap_or_exit(cli_options, EXIT_ERROR_UNDEFINED, &base_logger);

    // We aggregate the input files from the positional arguments and the glob
    // options. If we've no file input (either from the standard input or from
    // the command line arguments), we just print help and exit.
    let files = cli::get_strings(&matches, "FILE");
    let glob_files = &cli_options.glob_files;
    let filenames = get_input_files(&files, glob_files, &mut app, &base_logger);

    if cli_options.cookie_output_file.is_some() && filenames.len() > 1 {
        exit_with_error(
            "Only save cookies for a unique session",
            EXIT_ERROR_UNDEFINED,
            &base_logger,
        );
    }

    let progress_bar = cli_options.test && !verbose && !is_ci() && atty::is(Stream::Stderr);
    let current_dir = env::current_dir();
    let current_dir = unwrap_or_exit(current_dir, EXIT_ERROR_UNDEFINED, &base_logger);
    let current_dir = current_dir.as_path();

    let start = Instant::now();
    let mut runs = vec![];

    for (current, filename) in filenames.iter().enumerate() {
        // We check the input file existence and check that we can read its contents.
        // Once the preconditions succeed, we can parse the Hurl file, and run it.
        if filename != "-" && !Path::new(filename).exists() {
            let message = format!("hurl: cannot access '{filename}': No such file or directory");
            exit_with_error(&message, EXIT_ERROR_PARSING, &base_logger);
        }
        let content = cli::read_to_string(filename);
        let content = unwrap_or_exit(content, EXIT_ERROR_PARSING, &base_logger);

        let logger = LoggerBuilder::new()
            .color(color)
            .verbose(verbose)
            .test(cli_options.test)
            .progress_bar(progress_bar)
            .build();

        let total = filenames.len();
        logger.test_running(filename, current + 1, total);

        // We try to parse the text file to an HurlFile instance.
        let hurl_file = parser::parse_hurl_file(&content);
        if let Err(e) = hurl_file {
            logger.error_rich(filename, &content, &e);
            std::process::exit(EXIT_ERROR_PARSING);
        }

        // Now, we have a syntactically correct HurlFile instance, we can run it.
        let hurl_file = hurl_file.unwrap();
        let hurl_result = execute(
            &hurl_file,
            &content,
            filename,
            current_dir,
            &cli_options,
            &logger,
        );
        let success = hurl_result.success;

        logger.test_completed(&hurl_result, filename);

        // We can output the result, either the raw body or a structured JSON representation.
        let output_body = success
            && !cli_options.interactive
            && matches!(cli_options.output_type, cli::OutputType::ResponseBody);
        if output_body {
            let include_headers = cli_options.include;
            let result = output::write_body(
                &hurl_result,
                filename,
                include_headers,
                color,
                &cli_options.output,
                &logger,
            );
            unwrap_or_exit(result, EXIT_ERROR_RUNTIME, &base_logger);
        }

        if matches!(cli_options.output_type, cli::OutputType::Json) {
            let result = output::write_json(&hurl_result, &content, filename, &cli_options.output);
            unwrap_or_exit(result, EXIT_ERROR_RUNTIME, &base_logger);
        }

        let run = HurlRun {
            content,
            filename: filename.to_string(),
            hurl_file,
            hurl_result,
        };
        runs.push(run);
    }

    if let Some(filename) = cli_options.junit_file {
        base_logger.debug(format!("Writing JUnit report to {filename}").as_str());
        let result = create_junit_report(&runs, &filename);
        unwrap_or_exit(result, EXIT_ERROR_UNDEFINED, &base_logger);
    }

    if let Some(dir) = cli_options.html_dir {
        base_logger.debug(format!("Writing HTML report to {}", dir.display()).as_str());
        let result = create_html_report(&runs, &dir);
        unwrap_or_exit(result, EXIT_ERROR_UNDEFINED, &base_logger);
    }

    if let Some(filename) = cli_options.cookie_output_file {
        base_logger.debug(format!("Writing cookies to {filename}").as_str());
        let result = create_cookies_file(&runs, &filename);
        unwrap_or_exit(result, EXIT_ERROR_UNDEFINED, &base_logger);
    }

    if cli_options.test {
        let duration = start.elapsed().as_millis();
        let summary = get_summary(&runs, duration);
        base_logger.info(summary.as_str());
    }

    std::process::exit(exit_code(&runs));
}

/// Runs a Hurl file `hurl_file` and returns a result.
///
/// Original file `content` and `filename` are used to log rich asserts and errors
/// (including annotated source, line and column).
fn execute(
    hurl_file: &HurlFile,
    content: &str,
    filename: &str,
    current_dir: &Path,
    cli_options: &cli::CliOptions,
    logger: &Logger,
) -> HurlResult {
    log_run_info(hurl_file, cli_options, logger);

    let variables = &cli_options.variables;
    let cookie_input_file = cli_options.cookie_input_file.clone();
    let runner_options = cli_options.to(filename, current_dir);
    let mut client = http::Client::new(cookie_input_file);

    runner::run(
        hurl_file,
        content,
        filename,
        &mut client,
        &runner_options,
        variables,
        logger,
    )
}

/// Logs various debug information at the start of `hurl_file` run.
fn log_run_info(hurl_file: &HurlFile, cli_options: &cli::CliOptions, logger: &Logger) {
    logger.debug_important("Options:");
    logger.debug(format!("    fail fast: {}", cli_options.fail_fast).as_str());
    logger.debug(format!("    follow redirect: {}", cli_options.follow_location).as_str());
    logger.debug(format!("    insecure: {}", cli_options.insecure).as_str());
    if let Some(n) = cli_options.max_redirect {
        logger.debug(format!("    max redirect: {n}").as_str());
    }
    if let Some(proxy) = &cli_options.proxy {
        logger.debug(format!("    proxy: {proxy}").as_str());
    }
    logger.debug(format!("    retry: {}", cli_options.retry).as_str());
    if let Some(n) = cli_options.retry_max_count {
        logger.debug(format!("    retry max count: {n}").as_str());
    }
    if !cli_options.variables.is_empty() {
        logger.debug_important("Variables:");
        for (name, value) in cli_options.variables.iter() {
            logger.debug(format!("    {name}: {value}").as_str());
        }
    }
    if let Some(to_entry) = cli_options.to_entry {
        logger
            .debug(format!("Executing {}/{} entries", to_entry, hurl_file.entries.len()).as_str());
    }
}

#[cfg(target_family = "unix")]
pub fn init_colored() {
    control::set_override(true);
}

#[cfg(target_family = "windows")]
pub fn init_colored() {
    colored::control::set_override(true);
    colored::control::set_virtual_terminal(true).expect("set virtual terminal");
}

/// Unwraps a `result` or exit with message.
fn unwrap_or_exit<T, E>(result: Result<T, E>, code: i32, logger: &BaseLogger) -> T
where
    E: std::fmt::Display,
{
    match result {
        Ok(v) => v,
        Err(e) => exit_with_error(&e.to_string(), code, logger),
    }
}

/// Prints an error message and exits the current process with an exit code.
fn exit_with_error(message: &str, code: i32, logger: &BaseLogger) -> ! {
    if !message.is_empty() {
        logger.error(message);
    }
    std::process::exit(code);
}

/// Create a JUnit report for this run.
fn create_junit_report(runs: &[HurlRun], filename: &str) -> Result<(), cli::CliError> {
    let testcases: Vec<junit::Testcase> = runs
        .iter()
        .map(|r| junit::Testcase::from(&r.hurl_result, &r.content, &r.filename))
        .collect();
    junit::write_report(filename, &testcases)?;
    Ok(())
}

/// Create an HTML report for this run.
fn create_html_report(runs: &[HurlRun], dir_path: &Path) -> Result<(), cli::CliError> {
    let mut testcases = vec![];
    for run in runs.iter() {
        let testcase = html::Testcase::from(&run.hurl_result, &run.filename);
        testcase.write_html(&run.hurl_file, dir_path)?;
        testcases.push(testcase);
    }
    html::write_report(dir_path, &testcases)?;
    Ok(())
}

/// Returns an exit code for a list of HurlResult.
fn exit_code(runs: &[HurlRun]) -> i32 {
    let mut count_errors_runner = 0;
    let mut count_errors_assert = 0;
    for run in runs.iter() {
        let errors = run.hurl_result.errors();
        if errors.is_empty() {
        } else if errors.iter().filter(|e| !e.assert).count() == 0 {
            count_errors_assert += 1;
        } else {
            count_errors_runner += 1;
        }
    }
    if count_errors_runner > 0 {
        EXIT_ERROR_RUNTIME
    } else if count_errors_assert > 0 {
        EXIT_ERROR_ASSERT
    } else {
        EXIT_OK
    }
}

/// Returns the input files from the positional arguments and the glob options.
fn get_input_files(
    files: &Option<Vec<String>>,
    glob_files: &[String],
    app: &mut Command,
    logger: &BaseLogger,
) -> Vec<String> {
    let mut filenames = vec![];
    if let Some(values) = files {
        for value in values {
            filenames.push(value.to_string());
        }
    };
    for filename in glob_files {
        filenames.push(filename.to_string());
    }
    if filenames.is_empty() {
        if atty::is(Stream::Stdin) {
            let error = if app.print_help().is_err() {
                "Panic during printing help"
            } else {
                ""
            };
            exit_with_error(error, EXIT_ERROR_COMMANDLINE, logger);
        } else {
            filenames.push("-".to_string());
        }
    }
    filenames
}

fn create_cookies_file(runs: &[HurlRun], filename: &str) -> Result<(), cli::CliError> {
    let mut file = match std::fs::File::create(filename) {
        Err(why) => {
            return Err(cli::CliError {
                message: format!("Issue writing to {filename}: {why:?}"),
            });
        }
        Ok(file) => file,
    };
    let mut s = r#"# Netscape HTTP Cookie File
# This file was generated by hurl

"#
    .to_string();
    match runs.first() {
        None => {
            return Err(cli::CliError {
                message: "Issue fetching results".to_string(),
            });
        }
        Some(run) => {
            for cookie in run.hurl_result.cookies.clone() {
                s.push_str(cookie.to_string().as_str());
                s.push('\n');
            }
        }
    }

    if let Err(why) = file.write_all(s.as_bytes()) {
        return Err(cli::CliError {
            message: format!("Issue writing to {filename}: {why:?}"),
        });
    }
    Ok(())
}

fn get_summary(runs: &[HurlRun], duration: u128) -> String {
    let total = runs.len();
    let success = runs.iter().filter(|r| r.hurl_result.success).count();
    let failed = total - success;
    let mut s =
        "--------------------------------------------------------------------------------\n"
            .to_string();
    s.push_str(format!("Executed files:  {total}\n").as_str());
    s.push_str(
        format!(
            "Succeeded files: {} ({:.1}%)\n",
            success,
            100.0 * success as f32 / total as f32
        )
        .as_str(),
    );
    s.push_str(
        format!(
            "Failed files:    {} ({:.1}%)\n",
            failed,
            100.0 * failed as f32 / total as f32
        )
        .as_str(),
    );
    s.push_str(format!("Duration:        {duration} ms\n").as_str());
    s
}

/// Whether or not this running in a Continuous Integration environment.
/// Code borrowed from <https://github.com/rust-lang/cargo/blob/master/crates/cargo-util/src/lib.rs>
fn is_ci() -> bool {
    env::var("CI").is_ok() || env::var("TF_BUILD").is_ok()
}
