//! This module allows you to configure the default settings for all
//! tests. All data structures here are normally parsed from `@` comments
//! in the files. These comments still overwrite the defaults, although
//! some boolean settings have no way to disable them.

use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use spanned::Spanned;

use crate::aux_builds::AuxBuilder;
use crate::build_manager::BuildManager;
use crate::custom_flags::Flag;
pub use crate::parser::{Comments, Condition, Revisioned};
use crate::parser::{ErrorMatch, ErrorMatchKind, OptWithLine};
pub use crate::rustc_stderr::Level;
use crate::rustc_stderr::Message;
use crate::test_result::{Errored, TestOk, TestResult};
use crate::{
    core::strip_path_prefix, rustc_stderr, Config, Error, Errors, Mode, OutputConflictHandling,
};

/// All information needed to run a single test
pub struct TestConfig<'a> {
    /// The generic config for all tests
    pub config: Config,
    pub(crate) revision: &'a str,
    pub(crate) comments: &'a Comments,
    /// The path to the current file
    pub path: &'a Path,
    /// The path to the folder where to look for aux files
    pub aux_dir: &'a Path,
}

impl TestConfig<'_> {
    pub(crate) fn patch_out_dir(&mut self) {
        // Put aux builds into a separate directory per path so that multiple aux files
        // from different directories (but with the same file name) don't collide.
        let relative = strip_path_prefix(self.path.parent().unwrap(), &self.config.out_dir);

        self.config.out_dir.extend(relative);
    }

    /// Create a file extension that includes the current revision if necessary.
    pub fn extension(&self, extension: &str) -> String {
        if self.revision.is_empty() {
            extension.to_string()
        } else {
            format!("{}.{extension}", self.revision)
        }
    }

    /// The test's mode after applying all comments
    pub fn mode(&self) -> Result<Spanned<Mode>, Errored> {
        self.comments.mode(self.revision)
    }

    pub(crate) fn find_one<'a, T: 'a>(
        &'a self,
        kind: &str,
        f: impl Fn(&'a Revisioned) -> OptWithLine<T>,
    ) -> Result<OptWithLine<T>, Errored> {
        self.comments.find_one_for_revision(self.revision, kind, f)
    }

    /// All comments that apply to the current test.
    pub fn comments(&self) -> impl Iterator<Item = &'_ Revisioned> {
        self.comments.for_revision(self.revision)
    }

    pub(crate) fn collect<'a, T, I: Iterator<Item = T>, R: FromIterator<T>>(
        &'a self,
        f: impl Fn(&'a Revisioned) -> I,
    ) -> R {
        self.comments().flat_map(f).collect()
    }

    fn apply_custom(&self, cmd: &mut Command) {
        for rev in self.comments.for_revision(self.revision) {
            for flag in rev.custom.values() {
                flag.content.apply(cmd, self);
            }
        }
    }

    pub(crate) fn build_command(
        &self,
        build_manager: &BuildManager<'_>,
    ) -> Result<Command, Errored> {
        let TestConfig {
            config,
            revision,
            comments,
            path,
            aux_dir,
        } = self;
        let mut cmd = config.program.build(&config.out_dir);
        let extra_args = self.build_aux_files(aux_dir, build_manager)?;
        cmd.args(extra_args);
        cmd.arg(path);
        if !revision.is_empty() {
            cmd.arg(format!("--cfg={revision}"));
        }
        for arg in comments
            .for_revision(revision)
            .flat_map(|r| r.compile_flags.iter())
        {
            cmd.arg(arg);
        }

        self.apply_custom(&mut cmd);

        if let Some(target) = &config.target {
            // Adding a `--target` arg to calls to Cargo will cause target folders
            // to create a target-specific sub-folder. We can avoid that by just
            // not passing a `--target` arg if its the same as the host.
            if !config.host_matches_target() {
                cmd.arg("--target").arg(target);
            }
        }

        // False positive in miri, our `map` uses a ref pattern to get the references to the tuple fields instead
        // of a reference to a tuple
        #[allow(clippy::map_identity)]
        cmd.envs(
            comments
                .for_revision(revision)
                .flat_map(|r| r.env_vars.iter())
                .map(|(k, v)| (k, v)),
        );

        Ok(cmd)
    }

    pub(crate) fn output_path(&self, kind: &str) -> PathBuf {
        let ext = self.extension(kind);
        if self.comments().any(|r| r.stderr_per_bitwidth) {
            return self
                .path
                .with_extension(format!("{}bit.{ext}", self.config.get_pointer_width()));
        }
        self.path.with_extension(ext)
    }

    pub(crate) fn normalize(&self, text: &[u8], kind: &'static str) -> Vec<u8> {
        let mut text = text.to_owned();

        for (from, to) in self.comments().flat_map(|r| match kind {
            "fixed" => &[] as &[_],
            "stderr" => &r.normalize_stderr,
            "stdout" => &r.normalize_stdout,
            _ => unreachable!(),
        }) {
            text = from.replace_all(&text, to).into_owned();
        }
        text
    }

    pub(crate) fn check_test_output(&self, errors: &mut Errors, stdout: &[u8], stderr: &[u8]) {
        // Check output files (if any)
        // Check output files against actual output
        self.check_output(stderr, errors, "stderr");
        self.check_output(stdout, errors, "stdout");
    }

    pub(crate) fn check_output(
        &self,
        output: &[u8],
        errors: &mut Errors,
        kind: &'static str,
    ) -> PathBuf {
        let output = self.normalize(output, kind);
        let path = self.output_path(kind);
        match &self.config.output_conflict_handling {
            OutputConflictHandling::Error => {
                let expected_output = std::fs::read(&path).unwrap_or_default();
                if output != expected_output {
                    errors.push(Error::OutputDiffers {
                        path: path.clone(),
                        actual: output.clone(),
                        expected: expected_output,
                        bless_command: self.config.bless_command.clone(),
                    });
                }
            }
            OutputConflictHandling::Bless => {
                if output.is_empty() {
                    let _ = std::fs::remove_file(&path);
                } else {
                    std::fs::write(&path, &output).unwrap();
                }
            }
            OutputConflictHandling::Ignore => {}
        }
        path
    }

    fn check_test_result(
        &self,
        command: Command,
        output: Output,
    ) -> Result<(Command, Output), Errored> {
        let mut errors = vec![];
        errors.extend(self.mode()?.ok(output.status).err());
        // Always remove annotation comments from stderr.
        let diagnostics = rustc_stderr::process(self.path, &output.stderr);
        self.check_test_output(&mut errors, &output.stdout, &diagnostics.rendered);
        // Check error annotations in the source against output
        self.check_annotations(
            diagnostics.messages,
            diagnostics.messages_from_unknown_file_or_line,
            &mut errors,
        )?;
        if errors.is_empty() {
            Ok((command, output))
        } else {
            Err(Errored {
                command,
                errors,
                stderr: diagnostics.rendered,
                stdout: output.stdout,
            })
        }
    }

    pub(crate) fn check_annotations(
        &self,
        mut messages: Vec<Vec<Message>>,
        mut messages_from_unknown_file_or_line: Vec<Message>,
        errors: &mut Errors,
    ) -> Result<(), Errored> {
        let error_patterns = self.comments().flat_map(|r| r.error_in_other_files.iter());

        let mut seen_error_match = None;
        for error_pattern in error_patterns {
            seen_error_match = Some(error_pattern.span());
            // first check the diagnostics messages outside of our file. We check this first, so that
            // you can mix in-file annotations with //@error-in-other-file annotations, even if there is overlap
            // in the messages.
            if let Some(i) = messages_from_unknown_file_or_line
                .iter()
                .position(|msg| error_pattern.matches(&msg.message))
            {
                messages_from_unknown_file_or_line.remove(i);
            } else {
                errors.push(Error::PatternNotFound {
                    pattern: error_pattern.clone(),
                    expected_line: None,
                });
            }
        }
        let diagnostic_code_prefix = self
            .find_one("diagnostic_code_prefix", |r| {
                r.diagnostic_code_prefix.clone()
            })?
            .into_inner()
            .map(|s| s.content)
            .unwrap_or_default();

        // The order on `Level` is such that `Error` is the highest level.
        // We will ensure that *all* diagnostics of level at least `lowest_annotation_level`
        // are matched.
        let mut lowest_annotation_level = Level::Error;
        'err: for &ErrorMatch { ref kind, line } in
            self.comments().flat_map(|r| r.error_matches.iter())
        {
            match kind {
                ErrorMatchKind::Code(code) => {
                    seen_error_match = Some(code.span());
                }
                &ErrorMatchKind::Pattern { ref pattern, level } => {
                    seen_error_match = Some(pattern.span());
                    // If we found a diagnostic with a level annotation, make sure that all
                    // diagnostics of that level have annotations, even if we don't end up finding a matching diagnostic
                    // for this pattern.
                    if lowest_annotation_level > level {
                        lowest_annotation_level = level;
                    }
                }
            }

            if let Some(msgs) = messages.get_mut(line.get()) {
                match kind {
                    &ErrorMatchKind::Pattern { ref pattern, level } => {
                        let found = msgs
                            .iter()
                            .position(|msg| pattern.matches(&msg.message) && msg.level == level);
                        if let Some(found) = found {
                            msgs.remove(found);
                            continue;
                        }
                    }
                    ErrorMatchKind::Code(code) => {
                        for (i, msg) in msgs.iter().enumerate() {
                            if msg.level != Level::Error {
                                continue;
                            }
                            let Some(msg_code) = &msg.code else { continue };
                            let Some(msg) = msg_code.strip_prefix(&diagnostic_code_prefix) else {
                                continue;
                            };
                            if msg == **code {
                                msgs.remove(i);
                                continue 'err;
                            }
                        }
                    }
                }
            }

            errors.push(match kind {
                ErrorMatchKind::Pattern { pattern, .. } => Error::PatternNotFound {
                    pattern: pattern.clone(),
                    expected_line: Some(line),
                },
                ErrorMatchKind::Code(code) => Error::CodeNotFound {
                    code: Spanned::new(
                        format!("{}{}", diagnostic_code_prefix, **code),
                        code.span(),
                    ),
                    expected_line: Some(line),
                },
            });
        }

        let required_annotation_level = self
            .find_one("`require_annotations_for_level` annotations", |r| {
                r.require_annotations_for_level.clone()
            })?;

        let required_annotation_level = required_annotation_level
            .into_inner()
            .map_or(lowest_annotation_level, |l| *l);
        let filter = |mut msgs: Vec<Message>| -> Vec<_> {
            msgs.retain(|msg| msg.level >= required_annotation_level);
            msgs
        };

        let mode = self.mode()?;

        if !matches!(*mode, Mode::Yolo { .. }) {
            let messages_from_unknown_file_or_line = filter(messages_from_unknown_file_or_line);
            if !messages_from_unknown_file_or_line.is_empty() {
                errors.push(Error::ErrorsWithoutPattern {
                    path: None,
                    msgs: messages_from_unknown_file_or_line,
                });
            }

            for (line, msgs) in messages.into_iter().enumerate() {
                let msgs = filter(msgs);
                if !msgs.is_empty() {
                    let line = NonZeroUsize::new(line).expect("line 0 is always empty");
                    errors.push(Error::ErrorsWithoutPattern {
                        path: Some(Spanned::new(
                            self.path.to_path_buf(),
                            spanned::Span {
                                line_start: line,
                                ..spanned::Span::default()
                            },
                        )),
                        msgs,
                    });
                }
            }
        }

        match (*mode, seen_error_match) {
            (Mode::Pass, Some(span)) | (Mode::Panic, Some(span)) => {
                errors.push(Error::PatternFoundInPassTest {
                    mode: mode.span(),
                    span,
                })
            }
            (
                Mode::Fail {
                    require_patterns: true,
                    ..
                },
                None,
            ) => errors.push(Error::NoPatternsFound),
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn build_aux_files(
        &self,
        aux_dir: &Path,
        build_manager: &BuildManager<'_>,
    ) -> Result<Vec<OsString>, Errored> {
        let mut extra_args = vec![];
        for rev in self.comments() {
            for aux in &rev.aux_builds {
                build_aux_file(aux, aux_dir, &mut extra_args, build_manager)?;
            }
        }
        Ok(extra_args)
    }

    pub(crate) fn run_test(mut self, build_manager: &BuildManager<'_>) -> TestResult {
        self.patch_out_dir();

        let mut cmd = self.build_command(build_manager)?;
        let stdin = self.path.with_extension(self.extension("stdin"));
        if stdin.exists() {
            cmd.stdin(std::fs::File::open(stdin).unwrap());
        }

        let (cmd, output) = crate::core::run_command(cmd)?;

        let (mut cmd, output) = self.check_test_result(cmd, output)?;

        for rev in self.comments() {
            for custom in rev.custom.values() {
                if let Some(c) =
                    custom
                        .content
                        .post_test_action(&self, cmd, &output, build_manager)?
                {
                    cmd = c;
                } else {
                    return Ok(TestOk::Ok);
                }
            }
        }
        Ok(TestOk::Ok)
    }

    pub(crate) fn find_one_custom(&self, arg: &str) -> Result<OptWithLine<&dyn Flag>, Errored> {
        self.find_one(arg, |r| r.custom.get(arg).map(|s| s.as_ref()).into())
    }
}

fn build_aux_file(
    aux: &Spanned<PathBuf>,
    aux_dir: &Path,
    extra_args: &mut Vec<OsString>,
    build_manager: &BuildManager<'_>,
) -> Result<(), Errored> {
    let line = aux.line();
    let aux = &**aux;
    let aux_file = if aux.starts_with("..") {
        aux_dir.parent().unwrap().join(aux)
    } else {
        aux_dir.join(aux)
    };
    extra_args.extend(
        build_manager
            .build(AuxBuilder {
                aux_file: strip_path_prefix(
                    &aux_file.canonicalize().map_err(|err| Errored {
                        command: Command::new(format!(
                            "canonicalizing path `{}`",
                            aux_file.display()
                        )),
                        errors: vec![],
                        stderr: err.to_string().into_bytes(),
                        stdout: vec![],
                    })?,
                    &std::env::current_dir().unwrap(),
                )
                .collect(),
            })
            .map_err(
                |Errored {
                     command,
                     errors,
                     stderr,
                     stdout,
                 }| Errored {
                    command,
                    errors: vec![Error::Aux {
                        path: aux_file,
                        errors,
                        line,
                    }],
                    stderr,
                    stdout,
                },
            )?,
    );
    Ok(())
}
