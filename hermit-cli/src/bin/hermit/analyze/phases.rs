/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::bail;
use anyhow::Context;
use clap::Parser;
use colored::Colorize;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
use detcore::util::truncated;
use hermit::process::Bind;
use hermit::Error;
use rand::Rng;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;
use reverie::process::ExitStatus;
use reverie::process::Output;
use tracing::metadata::LevelFilter;

use crate::analyze::consts::*;
use crate::analyze::rundata::RunData;
use crate::analyze::types::AnalyzeOpts;
use crate::analyze::types::ExitStatusConstraint;
use crate::analyze::types::Report;
use crate::analyze::types::ReportCriticalEvent;
use crate::global_opts::GlobalOpts;
use crate::logdiff::LogDiffCLIOpts;
use crate::run::RunOpts;
use crate::schedule_search::search_for_critical_schedule;
use crate::schedule_search::CriticalSchedule;

/// Compare only preemptions, not recorded schedules.
fn preempt_files_equal(path1: &Path, path2: &Path) -> bool {
    let mut pr1 = PreemptionReader::new(path1).load_all();
    let mut pr2 = PreemptionReader::new(path2).load_all();
    pr1.preemptions_only();
    pr2.preemptions_only();
    pr1 == pr2
}

/// Return true the launched run matches the target criteria.
/// Also return the path to the log file that was written.
type LaunchResult = anyhow::Result<(bool, PathBuf)>;

fn yellow_msg(msg: &str) {
    eprintln!(":: {}", msg.yellow().bold());
}

impl AnalyzeOpts {
    fn log_path(&self, runname: &str) -> PathBuf {
        let tmp_dir = self.tmp_dir.as_ref().unwrap();
        tmp_dir.join(runname).with_extension(LOG_EXT)
    }

    fn preempts_path(&self, runname: &str) -> PathBuf {
        let tmp_dir = self.tmp_dir.as_ref().unwrap();
        tmp_dir.join(runname).with_extension(PREEMPTS_EXT)
    }

    fn _sched_path(&self, runname: &str) -> PathBuf {
        let tmp_dir = self.tmp_dir.as_ref().unwrap();
        tmp_dir.join(runname).with_extension(SCHED_EXT)
    }

    pub fn get_tmp(&self) -> anyhow::Result<&Path> {
        if let Some(pb) = &self.tmp_dir {
            Ok(pb.as_path())
        } else {
            bail!("Expected tmp_dir to be set at this point!")
        }
    }

    pub fn print_and_validate_runopts(&self, ro: &mut RunOpts, runname: &str) {
        if self.verbose {
            ro.summary = true;
            eprintln!(
                ":: [verbose] Repro command:\n{}",
                self.runopts_to_repro(ro, Some(runname))
            );
        }
        ro.validate_args();
    }

    // TODO: REMOVE
    /// Launch a single run with the given options.
    /// (Also set up logging and temp dir binding.)
    fn launch_config(&self, runname: &str, runopts: &mut RunOpts) -> LaunchResult {
        let root = self.get_tmp()?.join(runname);
        let log_path = self.log_path(runname);
        self.print_and_validate_runopts(runopts, runname);

        let conf_file = root.with_extension("config");
        runopts.save_config = Some(conf_file);

        let gopts = if self.verbose || self.selfcheck {
            GlobalOpts {
                log: Some(LevelFilter::DEBUG),
                log_file: Some(log_path.clone()),
            }
        } else {
            NO_LOGGING.clone()
        };

        let (_, output) = runopts.run(&gopts, true)?;
        let output: Output = output.context("expected captured output")?;

        File::create(root.with_extension("stdout"))
            .unwrap()
            .write_all(&output.stdout)
            .unwrap();
        File::create(root.with_extension("stderr"))
            .unwrap()
            .write_all(&output.stderr)
            .unwrap();

        let is_a_match = self.output_matches(&output);
        Ok((is_a_match, log_path))
    }

    /// Launch a chaos run searching for a target (e.g. failing) schudule.
    /// Returns Some if a target schedule is found.
    fn launch_search(&self, round: u64, sched_seed: u64) -> Result<Option<RunData>, Error> {
        yellow_msg(&format!(
            "Searching (round {}) for a target execution, chaos --sched-seed={} ",
            round, sched_seed
        ));

        let runname = format!("search_round_{:0wide$}", round, wide = 3);

        let mut ro = self.get_base_runopts()?;
        ro.det_opts.det_config.sched_seed = Some(sched_seed);
        if self.imprecise_search {
            ro.det_opts.det_config.imprecise_timers = true; // TODO: enable this by default when bugs are fixed.
        }
        let mut rundat = RunData::new(self, runname, ro).with_preemption_recording();

        rundat.launch()?;
        // let (is_a_match, _) = self.launch_config(&runname, &mut ro)?;
        if rundat.is_a_match() {
            eprintln!(
                ":: {}:\n    {}",
                "Target run established by --search. Reproducer"
                    .green()
                    .bold(),
                rundat.to_repro(),
            );
            Ok(Some(rundat))
        } else {
            Ok(None)
        }
    }

    // TODO: REMOVE
    /// Launch a single run with logging and preemption recording.  Return true if it matches the criteria.
    fn launch_and_record_preempts(
        &self,
        runname: &str,
        msg: &str,
        mut runopts: RunOpts,
    ) -> LaunchResult {
        yellow_msg(&format!("{} record preemptions and schedule...", msg));
        let preempts_path = self.preempts_path(runname);
        runopts.det_opts.det_config.record_preemptions = true;
        runopts.det_opts.det_config.record_preemptions_to = Some(preempts_path);
        self.launch_config(runname, &mut runopts)
    }

    // TODO: REMOVE. Only used by minimize atm.
    //
    /// Launch a run with preempts provided (to replay). No logging. Return true if it matches the
    /// criteria. If provided, additionally record full schedule events from the run to
    /// `record_sched_path`.
    pub(super) fn launch_from_preempts_to_sched(
        &self,
        runname: &str,
        preempts_path: &Path,
        record_sched_path: Option<&Path>,
    ) -> anyhow::Result<(bool, RunOpts)> {
        let mut ro = self.get_base_runopts()?;
        ro.det_opts.det_config.replay_preemptions_from = Some(preempts_path.to_path_buf());
        if let Some(path) = record_sched_path {
            ro.det_opts.det_config.record_preemptions_to = Some(path.to_path_buf());
        }
        let (is_a_match, _) = self.launch_config(runname, &mut ro)?;
        Ok((is_a_match, ro))
    }

    /// Runs the program with the specified schedule.
    /// Returns whether the final run met the criteria as expected.
    /// Also returns the paths to stack traces of the two critical events.
    fn launch_for_stacktraces(
        &self,
        runname: &str,
        schedule_path: &Path,
        critical_event_index: u64,
    ) -> anyhow::Result<(bool, PathBuf, PathBuf, RunOpts)> {
        let tmp_dir = self.get_tmp()?;
        let stack1_path = tmp_dir.join(runname).with_extension("stack1");
        let stack2_path = tmp_dir.join(runname).with_extension("stack2");

        let mut ro = self.get_base_runopts()?;
        ro.det_opts.det_config.replay_schedule_from = Some(schedule_path.to_path_buf());
        ro.det_opts.det_config.stacktrace_event = [
            (critical_event_index - 1, Some(stack1_path.clone())),
            (critical_event_index, Some(stack2_path.clone())),
        ]
        .to_vec();

        let (is_a_match, _log_path) = self.launch_config(runname, &mut ro)?;
        Ok((is_a_match, stack1_path, stack2_path, ro))
    }

    pub fn runopts_to_repro(&self, runopts: &RunOpts, runname: Option<&str>) -> String {
        if let Some(runname) = runname {
            let logging = if self.verbose || self.selfcheck {
                let path = self.log_path(runname);
                format!(" --log=debug --log-file={}", path.display())
            } else {
                "".to_string()
            };
            format!("hermit{} run {}", logging, runopts)
        } else {
            format!("hermit run {}", runopts)
        }
    }

    fn runopts_add_binds(&self, runopts: &mut RunOpts) -> anyhow::Result<()> {
        let bind_dir: Bind = Bind::from_str(self.get_tmp()?.to_str().unwrap())?;
        runopts.bind.push(bind_dir);
        runopts.validate_args();
        Ok(())
    }

    /// It's weird if no filter is specified.
    fn has_filters(&self) -> bool {
        self.target_stdout.is_some()
            || self.target_stderr.is_some()
            || self.target_exit_code != ExitStatusConstraint::Any
    }

    /// The raw, unvarnished, RunOpts.
    fn get_raw_runopts(&self) -> RunOpts {
        // Bogus arg 0 for CLI argument parsing:
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];

        for arg in &self.run_arg {
            run_cmd.push(arg.to_string());
        }
        for arg in &self.run_args {
            run_cmd.push(arg.to_string());
        }
        RunOpts::from_iter(run_cmd.iter())
    }

    /// The baseline RunOpts based on user flags plus some sanitation/validation.
    fn get_base_runopts(&self) -> anyhow::Result<RunOpts> {
        let mut ro = self.get_raw_runopts();
        if ro.no_sequentialize_threads {
            bail!(
                "Error, cannot search through executions with --no-sequentialize-threads.  Determinism required.",
            )
        }

        // We could add a flag for analyze-without chaos, but it's a rare use case that isn't
        // usefully supported now anyway.  Exploring with RNG alone doesn't make sense, but we may
        // want to make it possible to do analyze with the stick random scheduler instead of the
        // one.
        ro.det_opts.det_config.chaos = true;

        ro.validate_args();
        assert!(ro.det_opts.det_config.sequentialize_threads);
        if self.run1_seed.is_some() && !ro.det_opts.det_config.chaos {
            eprintln!(
                "{}",
                "WARNING: --chaos not in supplied hermit run args, but --run1-seed is.  Usually this is an error."
                    .bold()
                    .red()
            )
        }
        self.runopts_add_binds(&mut ro)?;

        Ok(ro)
    }

    /// Extract the (initial) RunOpts for run1 that are implied by all of hermit analyze's arguments.
    fn get_run1_runopts(&self) -> anyhow::Result<RunOpts> {
        let mut ro = self.get_base_runopts()?;
        // If there was a --sched-seed specified in run_args, it is overridden by this setting:
        if let Some(seed) = self.run1_seed {
            ro.det_opts.det_config.seed = seed;
        } else if let Some(path) = &self.run1_preemptions {
            ro.det_opts.det_config.replay_preemptions_from = Some(path.clone());
        }
        Ok(ro)
    }

    fn display_criteria(&self) -> String {
        let mut strs: Vec<String> = Vec::new();
        match &self.target_exit_code {
            ExitStatusConstraint::Exact(c) => {
                strs.push(format!("exit code={}", c));
            }
            ExitStatusConstraint::NonZero => {
                strs.push("nonzero exit".to_string());
            }
            ExitStatusConstraint::Any => {}
        }
        if self.target_stdout.is_some() {
            strs.push(" matching stdout".to_string());
        }
        if self.target_stderr.is_some() {
            strs.push(" matching stderr".to_string());
        }
        strs.join(", ")
    }

    /// Mutates AnalyzeOpts in order to initialize some fields.
    fn phase0_initialize(&mut self) -> anyhow::Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("hermit_analyze")
            .tempdir()?;
        let tmpdir_path = dir.into_path(); // For now always keep the temporary results.
        eprintln!(":: Temp workspace: {}", tmpdir_path.display());
        self.tmp_dir = Some(tmpdir_path);
        Ok(())
    }

    /// Create our workspace and verify the input run matches the criteria, or find one that does.
    /// Returns the results of the target run.
    fn phase1_establish_target_run(&self) -> Result<RunData, Error> {
        // Must run after tmp_dir is set:
        let run1_opts = self.get_run1_runopts()?;
        eprintln!(
            ":: {} hermit run {}",
            "Studying execution: ".yellow().bold(),
            run1_opts
        );

        let runname = "run1_target";
        let mut run1data = RunData::new(self, runname.to_string(), run1_opts);

        if let Some(p) = &self.run1_preemptions {
            let preempts_path = run1data.preempts_path_out();
            // Copy preempts to the output location as though they were recorded by this run:
            std::fs::copy(p, preempts_path).expect("copy file to succeed");
            // Careful, returning a NON-launched RunData just to contain the output path.
            return Ok(run1data);
        }

        // Translate the seed into a set of preemptions we can work from.
        let mut run1data = run1data.with_preemption_recording();
        run1data.launch()?;

        if !run1data.is_a_match() {
            if self.search {
                eprintln!(
                    ":: {}",
                    "First run did not match target criteria; now searching for a matching run..."
                        .red()
                        .bold()
                );
                Ok(self.do_search())
            } else {
                bail!("FAILED. The run did not match the target criteria. Try --search.");
            }
        } else if self.has_filters() {
            eprintln!(
                ":: {}",
                format!(
                    "First run matched target criteria ({}).",
                    self.display_criteria()
                )
                .green()
                .bold(),
            );
            Ok(run1data)
        } else {
            eprintln!(":: {}", "WARNING: run without any --filter arguments, so accepting ALL runs. This is probably not what you wanted.".red().bold());
            Ok(run1data)
        }
    }

    /// Reduce the set of preemptions needed to match the criteria.
    ///
    /// Takes the input (non-minimized) preemptions as a file path and returns the minimized
    /// preemptions as a data structure in memory.
    ///
    /// # Returns
    /// - The on-target run with minimized schedule.
    fn phase2_minimize(
        &self,
        global: &GlobalOpts,
        mut last_run: RunData,
    ) -> anyhow::Result<RunData> {
        if self.minimize {
            // In this scenario we need to work with preemptions.

            let preempts_path = last_run.preempts_path_out();
            let (min_pr, min_pr_path, min_log_path) = self.minimize(preempts_path, global)?;
            eprintln!(
                ":: {}\n {}",
                "Successfully minimized to these critical interventions:"
                    .green()
                    .bold(),
                truncated(1000, serde_json::to_string(&min_pr).unwrap())
            );

            // TEMP: construct a RunData post-facto, until we finish the minimize overhaul:
            let runname = "minimized".to_string();
            let min_run = RunData::from_minimize_output(
                self,
                runname,
                last_run.runopts.clone(),
                min_pr,
                min_pr_path,
                min_log_path,
            );
            Ok(min_run)
        } else {
            // In this scenario we only care about event traces, and never really need to work with
            // preemptions.  Still, we'll need to do another run to record the trace.
            Ok(last_run)
        }
    }

    fn _log_diff(
        &self,
        global: &GlobalOpts,
        run1_log_path: &Path,
        run2_log_path: &Path,
    ) -> ExitStatus {
        if self.verbose {
            yellow_msg("[comparing] with log-diff command:");
            eprintln!(
                "    hermit log-diff {} {}",
                run1_log_path.display(),
                run2_log_path.display(),
            );
        }
        let ldopts = LogDiffCLIOpts::new(run1_log_path, run2_log_path);
        ldopts.main(global)
    }

    /// A weaker log difference that does not expect certain lines to be conserved in preemption replay.
    fn log_diff_preemption_replay(
        &self,
        global: &GlobalOpts,
        run1_log_path: &Path,
        run2_log_path: &Path,
    ) -> ExitStatus {
        if self.verbose {
            yellow_msg("[comparing] with log-diff command:");
            eprintln!(
                "    hermit log-diff --ignore-lines=CHAOSRAND {} {}",
                run1_log_path.display(),
                run2_log_path.display(),
            );
        }
        let mut ldopts = LogDiffCLIOpts::new(run1_log_path, run2_log_path);
        ldopts.more.ignore_lines = vec!["CHAOSRAND".to_string(), "SCHEDRAND".to_string()];
        ldopts.main(global)
    }

    /// Optionally do an extra run to verify that preemptions replay and yield the exact same
    /// execution.a
    pub fn phase3_strict_preempt_replay_check(
        &self,
        global: &GlobalOpts,
        min_run: &mut RunData,
    ) -> Result<(), Error> {
        let min_log_path = &min_run.log_path().unwrap().to_path_buf();
        let min_preempts_path = min_run.preempts_path_out();

        if self.selfcheck {
            yellow_msg("[selfcheck] Verifying target run preserved under preemption-replay");

            let mut run1b_opts = self.get_run1_runopts()?;
            run1b_opts.det_opts.det_config.replay_preemptions_from =
                Some(min_preempts_path.to_path_buf());
            let runname = "run1b_selfcheck";
            eprintln!("    {}", self.runopts_to_repro(&run1b_opts, Some(runname)));

            let (second_matches, _log_path) = self.launch_and_record_preempts(
                runname,
                "[selfcheck] Additional (target) run, replaying preemptions:",
                run1b_opts,
            )?;

            yellow_msg("[selfcheck] Comparing output from additional run (run1 vs run1b)");
            let run1b_log_path = self.log_path(runname);
            let status = self.log_diff_preemption_replay(global, min_log_path, &run1b_log_path);
            if !second_matches {
                bail!("First run matched criteria but second run did not.");
            }
            if !status.success() {
                bail!(
                    "Log differences found, aborting because --selfcheck requires perfect reproducibility of the target run!"
                )
            }
            let run1b_preempts_path = self.preempts_path(runname);
            if !preempt_files_equal(min_preempts_path, &run1b_preempts_path) {
                bail!(
                    "The preemptions recorded by the additional run did not match the preemptions replayed (no fixed point): {} vs {}",
                    min_preempts_path.display(),
                    run1b_preempts_path.display(),
                );
            }

            eprintln!(
                ":: {}",
                "Identical executions confirmed between target run and its preemption-based replay."
                    .green()
                    .bold()
            );
        }
        Ok(())
    }

    /// Once we have the target MATCHING run in hand (usually crashing/failing), we need to
    /// determine which baseline, non-matching run to use. Then we need to extract the schedule from
    /// it.
    ///
    /// Returns a path to a file containing recorded schedule events for the baseline run.
    pub fn phase4_choose_baseline_sched_events(
        &self,
        matching_pr: PreemptionRecord,
    ) -> anyhow::Result<RunData> {
        if self.minimize {
            // Enforced by the clap conflicts_with annotations:
            assert!(self.run2_seed.is_none());
            assert!(self.run2_preemptions.is_none());
            assert!(self.run2_schedule.is_none());

            // If we're minimizing, then we know that ALL interventions in the schedule are critical.
            // Thus omitting any of them is sufficient to exit the target schedule space.
            // Omitting the last one should yield the lowest distance match/non-match schedule pair.
            let mut pr = matching_pr;
            let mut ix = 1;
            loop {
                let runname = format!("run2_baseline_try{:03}", ix);
                let (still_matching, mut newrun) =
                    self.save_nearby_non_matching_sched_events(&runname, &pr)?;
                if still_matching {
                    pr = newrun.preempts_out().clone();
                    ix += 1;
                } else {
                    return Ok(newrun);
                }
            }
        } else {
            // Tweak the runopts according to several different scenarios:
            let mut ro = self.get_base_runopts()?;
            let from_where;
            if let Some(seed) = self.run2_seed {
                // Replay from seed to record schedule.
                ro.det_opts.det_config.seed = seed;
                from_where = "--run2-seed";
            } else if let Some(path) = &self.run2_preemptions {
                // Replay from preemption record to record schedule.
                ro.det_opts.det_config.replay_preemptions_from = Some(path.clone());
                // TODO: if the file already contains a schedule, we don't need to rerun it.
                // Unless selfcheck is specified.
                from_where = "--run2-preemptions";
            } else if let Some(path) = &self.run2_schedule {
                if self.selfcheck {
                    // TODO: Don't trust the recorded schedule is a baseline, replay and check it.
                }
                let runname = "from_existing_run2_schedule".to_string();
                let fakerun = RunData::from_schedule_trace(self, runname, ro, path.clone());
                return Ok(fakerun);
            } else {
                from_where = "non-chaos (0 extra preemptions) run";

                // Otherwise we just assume that a *baseline* (non-chaos) run will do the trick.
                ro.det_opts.det_config.chaos = false;

                // Alternative:
                // We use the empty preemption record to basically approximate a non-chaos run.
                // let empty_pr = matching_pr.clone().strip_contents();
                // let empty_path = self
                //     .get_tmp()
                //     .join("empty_preempts")
                //     .with_extension(PREEMPTS_EXT);
                // empty_pr.write_to_disk(&empty_path).unwrap();
                // ro.det_opts.det_config.replay_preemptions_from = Some(empty_path.to_path_buf());
            }
            let runname = "run2_baseline".to_string();
            let mut newrun = RunData::new(self, runname, ro).with_preemption_recording();
            newrun.launch()?;

            eprintln!(
                ":: Recorded schedule from {} as baseline run ({})",
                from_where,
                newrun
                    .sched_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
            );
            if newrun.is_a_match() {
                bail!(
                    "Expectations not met... baseline run matched target criteria when it should not."
                )
            } else {
                eprintln!("Good: baseline run does not match criteria (e.g. pass not fail).");
                Ok(newrun)
            }
        }
    }

    /// Perform the binary search through schedule-space, identifying critical events.
    pub fn phase5_bisect_traces(
        &self,
        target: &[SchedEvent],
        baseline: &[SchedEvent],
    ) -> anyhow::Result<CriticalSchedule> {
        let mut i = 0;

        let base_opts = self.get_base_runopts()?;
        let test_fn = |sched: &[SchedEvent]| {
            i += 1;
            let runname = format!("bisect_round_{:0wide$}", i, wide = 3);

            // Prepare the next synthetic schedule on disk:
            let sched_path = self.get_tmp().unwrap().join(format!("{}.events", &runname));
            let next_sched = PreemptionRecord::from_sched_events(sched.to_owned());
            next_sched.write_to_disk(&sched_path).unwrap();

            let mut runopts = base_opts.clone();
            runopts.det_opts.det_config.replay_schedule_from = Some(sched_path);
            if self.verbose {
                eprintln!(
                    ":: {}, repro command:\n    {}",
                    format!("Testing execution during search (#{})", i)
                        .yellow()
                        .bold(),
                    self.runopts_to_repro(&runopts, Some(&runname)),
                );
            }
            let (is_match, _log_path) = self
                .launch_config(&runname, &mut runopts)
                .expect("Run failure");
            if is_match {
                eprintln!(" => Target condition ({})", self.display_criteria());
            } else {
                eprintln!(" => Baseline condition (usually absence of crash)");
            }
            (!is_match, sched.to_owned())
        };

        let target = target.to_vec(); // TODO: have search_for_critical_schedule borrow only.
        let baseline = baseline.to_vec();
        let crit = search_for_critical_schedule(test_fn, baseline, target, self.verbose);
        eprintln!(
            "Critical event of final on-target schedule is {}",
            crit.critical_event_index
        );
        Ok(crit)
    }

    /// Record the schedules on disk as reproducers and report stack-traces of critical events.
    pub fn phase6_record_outputs(&self, crit: CriticalSchedule) -> Result<Report, Error> {
        let tmp_dir = self.get_tmp()?;
        let CriticalSchedule {
            failing_schedule,
            passing_schedule,
            critical_event_index,
        } = crit;

        let runname = "final_target_for_stacktraces";
        let final_failing_path = tmp_dir.join(runname).with_extension(SCHED_EXT);
        {
            let pr = PreemptionRecord::from_sched_events(failing_schedule);
            pr.write_to_disk(&final_failing_path).unwrap();
            eprintln!(
                "Wrote final on-target ({}) schedule to {}",
                self.display_criteria(),
                final_failing_path.display()
            );
            let final_passing_path = tmp_dir.join("final_baseline").with_extension(SCHED_EXT);
            let pr = PreemptionRecord::from_sched_events(passing_schedule);
            pr.write_to_disk(&final_passing_path).unwrap();
            eprintln!(
                "Wrote final baseline (off-target) schedule to {}",
                final_passing_path.display()
            );
        }

        {
            let mut header = String::new();
            header.push_str(
                "These two operations, on different threads, are RACING with eachother.\n",
            );
            header.push_str(&format!(
                "The current order of events {} and {} is causing a FAILURE.\n",
                critical_event_index - 1,
                critical_event_index
            ));
            header.push_str(
                "You must add synchronization to prevent these operations from racing, or give them a different order.\n",
            );

            eprintln!(
                "\n:: {}",
                "Final run to print stack traces.  Repro command:"
                    .green()
                    .bold()
            );
            let (res, stack1_path, stack2_path, runopts) = self.launch_for_stacktraces(
                runname,
                &final_failing_path,
                critical_event_index as u64,
            )?;
            eprintln!("{}", self.runopts_to_repro(&runopts, Some(runname)));

            let stack1_file = File::open(stack1_path).unwrap();
            let stack1 = serde_json::from_reader(stack1_file).unwrap();
            let stack2_file = File::open(stack2_path).unwrap();
            let stack2 = serde_json::from_reader(stack2_file).unwrap();

            if res {
                // Also print to the screen:
                println!(
                    "\n------------------------------ hermit analyze report ------------------------------"
                );
                println!("{}", header);
                println!("{}", stack1);
                println!("{}", stack2);
                eprintln!(":: {}", "Completed analysis successfully.".green().bold());
                Ok(Report {
                    critical_event1: ReportCriticalEvent {
                        event_index: critical_event_index - 1,
                        stack: stack1,
                    },
                    critical_event2: ReportCriticalEvent {
                        event_index: critical_event_index,
                        stack: stack2,
                    },
                })
            } else {
                bail!("Internal error! Final run did NOT match the criteria as expected!")
            }
        }
    }

    pub fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Not implemented yet:
        if self.run1_schedule.is_some() {
            todo!()
        }
        if self.run2_schedule.is_some() {
            todo!()
        }

        if !self.get_raw_runopts().det_opts.det_config.chaos {
            eprintln!(
                ":: {} You may want to turn it on explicitly, along with a --preemption-timeout that works well for this program.",
                "WARNING: implicitly activating --chaos.".yellow().bold()
            );
        }

        self.phase0_initialize()?;
        let run1data = self.phase1_establish_target_run()?;

        let mut min_run = self.phase2_minimize(global, run1data)?;
        self.phase3_strict_preempt_replay_check(global, &mut min_run)?;

        min_run.normalize_preempts_out();
        let normalized_preempts = min_run.preempts_out().clone();
        eprintln!(
            ":: {}\n {}",
            &format!(
                "Normalized, that preemption record ({}) becomes:",
                min_run
                    .preempts_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
            )
            .green()
            .bold(),
            truncated(
                1000,
                serde_json::to_string_pretty(&normalized_preempts.clone_preemptions_only())
                    .unwrap()
            )
        );

        // One endpoint of the bisection search:
        let mut target_endpoint_run = self.save_final_target_sched_events(min_run)?;

        // The other endpoint of the bisection search:
        // What we thought was the final_pr can change here:
        let mut baseline_endpoint_run =
            self.phase4_choose_baseline_sched_events(normalized_preempts)?;

        let baseline_sched_name = baseline_endpoint_run.sched_out_file_name();
        let target_sched_name = target_endpoint_run.sched_out_file_name();
        let target = target_endpoint_run.sched_out();
        let baseline = baseline_endpoint_run.sched_out();

        eprintln!(
            ":: {} (event lengths {} / {}): {} {}",
            "Beginning bisection using endpoints".yellow().bold(),
            baseline.len(),
            target.len(),
            baseline_sched_name,
            target_sched_name,
        );
        let crit_sched = self.phase5_bisect_traces(target, baseline)?;

        let report = self.phase6_record_outputs(crit_sched)?;
        if let Some(path) = &self.report_file {
            let txt = serde_json::to_string(&report).unwrap();
            std::fs::write(path, txt).expect("Unable to write report file");
            eprintln!(
                ":: {}\n {}",
                "Final analysis report written to:".green().bold(),
                path.display()
            );
        }
        self.success_exit_code
            .map_or(Ok(ExitStatus::SUCCESS), |exit_code| {
                Ok(ExitStatus::Exited(exit_code))
            })
    }

    /// This extra run, to record the schedule, thus converting Preemptions to a full Schedule
    /// would be unnecessary if we recorded that each time as we minimize.
    ///
    /// Argument: the last on-target run that was preemption based.
    fn save_final_target_sched_events(&self, mut last_run: RunData) -> anyhow::Result<RunData> {
        let pr = last_run.preempts_out();
        if pr.contains_schedevents() {
            let path = last_run.preempts_path_out();
            // This run already has sched events recorded... no extra run necessary.
            yellow_msg(&format!(
                "Using the last run as the target endpoint ({} contains sched events)",
                path.file_name().unwrap().to_string_lossy()
            ));
            Ok(last_run)
        } else {
            // Verify that the new preemption record does in fact now cause a matching execution,
            // and rerecord during this verification with full recording that include sched events
            yellow_msg(
                "Verify target endpoint preemption record causes criteria to hold and record sched events",
            );
            let runname = "verify_target_endpoint";
            let mut newrun = RunData::new(self, runname.to_string(), last_run.runopts.clone())
                .with_preempts_path_in(last_run.preempts_path_out().to_path_buf())
                .with_schedule_recording();
            newrun.launch()?;
            eprintln!("    {}", newrun.to_repro());
            if !newrun.is_a_match() {
                bail!("Final preemption record still does not match target criteria");
            }
            Ok(newrun)
        }
    }

    // Returns the record, with one knockout, if it still satisfies the criteria that we want it not to.
    fn save_nearby_non_matching_sched_events(
        &self,
        runname: &str,
        matching_preempts: &PreemptionRecord,
    ) -> anyhow::Result<(bool, RunData)> {
        // Given preemptions that hermit analyze has determined are critical to match the criteria
        // (most commonly, a failing execution), removing the last critical preemption should
        // cause the minimal execution change to now no longer match the criteria (most commonly,
        // an execution that now passes).
        let non_matching_preempts = matching_preempts.with_latest_preempt_removed();

        // Validate the preemption record
        if let Err(e) = non_matching_preempts.validate() {
            bail!(
                "Hermit analyzer produced corrupt nearby non-matching preemption record, cannot proceed.\n\
                Error: {}\n\n\
                Corrupt record: {}",
                e,
                non_matching_preempts,
            );
        }

        let ro = self.get_base_runopts()?;
        let mut newrun = RunData::new(self, runname.to_string(), ro)
            .with_preempts_in(non_matching_preempts)
            .with_schedule_recording();

        // Verify that the new preemption record does in fact now cause a non-matching execution,
        // and rerecord during this verification with full recording that include sched events
        yellow_msg(
            "Verify preemption record *without* latest critical preempt causes criteria non-match. Also record sched events.",
        );
        newrun.launch()?;
        eprintln!("    {}", newrun.to_repro());
        if newrun.is_a_match() {
            eprintln!(
                "{}",
                ":: New preemption record still matches criteria! Attempting further knockouts.."
                    .red()
                    .bold(),
            );
            Ok((true, newrun))
        } else {
            Ok((false, newrun))
        }
    }

    /// Search for a target run. Return the run when found.
    fn do_search(&self) -> RunData {
        let search_seed = self.analyze_seed.unwrap_or_else(|| {
            let mut rng0 = rand::thread_rng();
            let seed: u64 = rng0.gen();
            yellow_msg(&format!("WARNING: performing --search with system randomness, use --analyze-seed={} to repro.", seed));
            seed
        });
        yellow_msg(&format!("Failure search using RNG seed {}", search_seed));
        let mut rng = Pcg64Mcg::seed_from_u64(search_seed);

        let mut round = 0;
        loop {
            let sched_seed = rng.gen();
            if let Some(mut rundat) = self
                .launch_search(round, sched_seed)
                .unwrap_or_else(|e| panic!("Error: {}", e))
            {
                if self.verbose {
                    let preempts = rundat.preempts_path_out();
                    let init_schedule: PreemptionRecord =
                        PreemptionReader::new(preempts).load_all();
                    eprintln!(
                        ":: {}:\nSchedule:\n {}",
                        "Search successfully found a failing run with schedule:"
                            .green()
                            .bold(),
                        truncated(
                            1000,
                            serde_json::to_string(&init_schedule.clone_preemptions_only()).unwrap()
                        ),
                    );
                }
                return rundat;
            }
            round += 1;
        }
    }

    /// Does the run meet the criteria we are looking for (e.g. a particular error message).
    pub fn output_matches(&self, out: &Output) -> bool {
        let mut answer = true;
        if let Some(pat) = &self.target_stdout {
            let str = String::from_utf8_lossy(&out.stdout);
            if !pat.is_match(&str) {
                if self.verbose {
                    eprintln!("Mismatch for stdout pattern {}", pat);
                    eprintln!("Stdout:\n{}", str);
                }
                answer = false;
            }
        }
        if let Some(pat) = &self.target_stderr {
            let str = String::from_utf8_lossy(&out.stderr);
            if self.verbose {
                eprintln!("Mismatch for stderr pattern {}", pat);
            }
            if !pat.is_match(&str) {
                answer = false;
            }
        }

        if !self.target_exit_code.is_match(out.status) {
            if self.verbose {
                eprintln!(
                    "  Exit code {} is not what we're looking for.",
                    out.status.into_raw()
                );
            }
            answer = false;
        }
        answer
    }
}
