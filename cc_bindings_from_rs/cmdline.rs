// Part of the Crubit project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[clap(name = "cc_bindings_from_rs")]
#[clap(about = "Generates C++ bindings for a Rust crate", long_about = None)]
pub struct Cmdline {
    /// Output path for C++ header file with bindings.
    #[clap(long, value_parser, value_name = "FILE")]
    pub h_out: PathBuf,

    /// Command line arguments of the Rust compiler.
    #[clap(last = true, value_parser)]
    pub rustc_args: Vec<String>,
}

impl Cmdline {
    pub fn new(args: &[String]) -> Result<Self> {
        assert_ne!(
            0,
            args.len(),
            "`args` should include the name of the executable (i.e. argsv[0])"
        );
        let exe_name = args[0].clone();

        // Ensure that `@file` expansion also covers *our* args.
        //
        // TODO(b/254688847): Decide whether to replace this with a `clap`-declared,
        // `--help`-exposed `--flagfile <path>`.
        let args = rustc_driver::args::arg_expand_all(args);

        // Parse `args` using the parser `derive`d by the `clap` crate.
        let mut cmdline = Self::try_parse_from(args)?;

        // For compatibility with `rustc_driver` expectations, we prepend `exe_name` to
        // `rustc_args.  This is needed, because `rustc_driver::RunCompiler::new`
        // expects that its `at_args` includes the name of the executable -
        // `handle_options` in `rustc_driver/src/lib.rs` throws away the first
        // element.
        cmdline.rustc_args.insert(0, exe_name);

        Ok(cmdline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use itertools::Itertools;
    use std::path::Path;
    use tempfile::tempdir;

    fn new_cmdline<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<Cmdline> {
        // When `Cmdline::new` is invoked from `main.rs`, it includes not only the
        // "real" cmdline arguments, but also the name of the executable.
        let args = std::iter::once("cc_bindings_from_rs_unittest_executable")
            .chain(args)
            .map(|s| s.to_string())
            .collect_vec();
        Cmdline::new(&args)
    }

    #[test]
    fn test_h_out_happy_path() {
        let cmdline = new_cmdline(["--h-out=foo.h"]).expect("This is a happy path");
        assert_eq!(Path::new("foo.h"), cmdline.h_out);
    }

    #[test]
    fn test_rustc_args_happy_path() {
        // Note that this test would fail without the `--` separator.
        let cmdline = new_cmdline(["--h-out=foo.h", "--", "test.rs", "--crate-type=lib"])
            .expect("This is a happy path");
        let rustc_args = &cmdline.rustc_args;
        assert!(
            itertools::equal(
                ["cc_bindings_from_rs_unittest_executable", "test.rs", "--crate-type=lib"],
                rustc_args
            ),
            "rustc_args = {:?}",
            rustc_args,
        );
    }

    #[test]
    fn test_help() {
        // This test has multiple purposes:
        // - Direct/obvious purpose: testing that `--help` works
        // - Double-checking the overall shape of our cmdline "API" (i.e. verification that the way
        //   we use `clap` attributes results in the desired cmdline "API"). This is a good enough
        //   coverage to avoid having flag-specifc tests (e.g. avoiding hypothetical
        //   `test_h_out_missing_flag`, `test_h_out_missing_arg`, `test_h_out_duplicated`).
        // - Exhaustively checking runtime asserts (assumming that tests run in a debug
        //   build; other tests also trigger these asserts).  See also:
        //     - https://github.com/clap-rs/clap/issues/2740#issuecomment-907240414
        //     - `clap::builder::App::debug_assert`
        let anyhow_err = new_cmdline(["--help"]).expect_err("--help should trigger an error");
        let clap_err = anyhow_err.downcast::<clap::Error>().expect("Expecting `clap` error");
        let expected_msg = r#"cc_bindings_from_rs 
Generates C++ bindings for a Rust crate

USAGE:
    cc_bindings_from_rs_unittest_executable --h-out <FILE> [-- <RUSTC_ARGS>...]

ARGS:
    <RUSTC_ARGS>...    Command line arguments of the Rust compiler

OPTIONS:
        --h-out <FILE>    Output path for C++ header file with bindings
    -h, --help            Print help information
"#;
        assert_eq!(expected_msg, clap_err.to_string());
    }

    #[test]
    fn test_here_file() -> anyhow::Result<()> {
        let tmpdir = tempdir()?;
        let tmpfile = tmpdir.path().join("herefile");
        std::fs::write(
            &tmpfile,
            ["--h-out=foo.h", "--", "test.rs", "--crate-type=lib"].join("\n"),
        )?;

        let flag_file_arg = format!("@{}", tmpfile.display());
        let cmdline = new_cmdline([flag_file_arg.as_str()]).expect("No errors expected");
        assert_eq!(Path::new("foo.h"), cmdline.h_out);
        let rustc_args = &cmdline.rustc_args;
        assert!(
            itertools::equal(
                ["cc_bindings_from_rs_unittest_executable", "test.rs", "--crate-type=lib"],
                rustc_args),
            "rustc_args = {:?}",
            rustc_args,
        );
        Ok(())
    }
}
