//! Process lifecycle, asynchronous acquisition and signal handling.
use super::{
    Controller, Options,
    reactor::{Reactor, SignalBatch},
    schedule::{ControlSchedule, Phase},
};
use crate::{
    config::Config,
    hardware::{ReadWorkers, WorkClass},
    policy::DecisionReason,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Instant,
};
/// Hold the inode lock for the process lifetime. Never unlink it: that would let
/// a second process lock a new inode while another still owns the old one.
pub fn acquire_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening instance lock {}", path.display()))?;
    file.try_lock()
        .context("another thinkfan-next instance is already running")?;
    Ok(file)
}

pub fn run(path: &Path, options: Options, once: bool) -> Result<()> {
    let _span = tracing::info_span!("controller", config = %path.display()).entered();
    // Register signals before device preparation so signals arriving during
    // startup are queued and handled before the first fan control cycle.
    let mut reactor = Reactor::new().context("initializing signal reactor")?;
    let workers = ReadWorkers::new(reactor.waker.clone())?;
    let initial = prepare_in_worker(&workers, Config::load(path)?, options.clone(), false)?
        .context("no worker available for startup")?;
    let mut paused = false;
    let mut deferred = SignalBatch::default();
    let mut controller = loop {
        let signals = reactor.wait(None)?;
        if signals.stop {
            return Ok(());
        }
        if signals.sleep {
            paused = true;
        }
        if signals.wake {
            paused = false;
        }
        deferred.reload |= signals.reload;
        deferred.dump |= signals.dump;
        match initial.try_recv() {
            Ok(result) => break result?,
            Err(TryRecvError::Empty) => (),
            Err(TryRecvError::Disconnected) => anyhow::bail!("startup worker disconnected"),
        }
    };
    reactor.defer(deferred);
    let result = run_loop(
        path,
        &options,
        once,
        &mut reactor,
        &mut controller,
        &workers,
        paused,
    );
    if result.is_err() {
        controller.emergency_all();
    }
    result
}

/// Preparation only probes/reads; returned fan owners are still Released.
fn prepare_in_worker(
    workers: &ReadWorkers,
    config: Config,
    options: Options,
    check: bool,
) -> Result<Option<Receiver<Result<Controller>>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    Ok(workers
        .submit(WorkClass::Prepare, || {
            Box::new(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut next = Controller::prepare(config, options)?;
                    if check {
                        next.check_hardware()?;
                    }
                    Ok(next)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("hardware preparation panicked")));
                let _ = sender.send(result);
            })
        })?
        .then_some(receiver))
}

fn run_loop(
    path: &Path,
    options: &Options,
    once: bool,
    reactor: &mut Reactor,
    controller: &mut Controller,
    workers: &ReadWorkers,
    paused: bool,
) -> Result<()> {
    let mut reload: Option<(Receiver<Result<Controller>>, Instant)> = None;
    let mut schedule = ControlSchedule::new(Instant::now(), paused);
    let mut awaiting_initial =
        Some(Instant::now() + controller.sensors.initial_timeout(options.interval));
    let once_deadline = Instant::now() + controller.sensors.initial_timeout(options.interval);
    let mut wake_deadline = None;
    let mut last_inputs = Vec::new();
    let mut inputs = Vec::new();
    let mut last_faults = (false, false);
    loop {
        let deadline = schedule.deadline(reload.as_ref().map(|(_, end)| *end));
        let signals = reactor
            .wait(deadline)
            .context("waiting for control events")?;
        workers.check_health()?;
        if signals.stop {
            tracing::info!("stopping fan control");
            return controller.restore();
        }
        if signals.sleep {
            controller.restore()?;
            controller.reset();
            schedule.phase = Phase::Paused;
            tracing::info!("fan control released for sleep");
        }
        if signals.wake {
            controller.reset();
            awaiting_initial =
                Some(Instant::now() + controller.sensors.initial_timeout(options.interval));
            wake_deadline = Some(Instant::now() + options.interval * 4);
            schedule.phase = Phase::Sampling(Instant::now());
            tracing::info!("fan control resumed");
        }
        if signals.reload {
            // Finish any active pulse before replacing its speed snapshot.
            if matches!(schedule.phase, Phase::Depulsing(_)) {
                controller.depulse_end()?;
                schedule.phase = Phase::Sampling(Instant::now());
            }
            if reload.is_none() {
                match Config::load(path) {
                    Ok(config) => {
                        reload = prepare_in_worker(workers, config, options.clone(), true)?
                            .map(|receiver| (receiver, Instant::now() + options.interval * 2));
                        if reload.is_none() {
                            tracing::warn!(
                                "reload deferred: preparation worker occupied; resend HUP"
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %format_args!("{error:#}"), "reload rejected; keeping previous configuration")
                    }
                }
            } else {
                tracing::debug!("reload already in progress");
            }
        }
        if let Some((receiver, until)) = &reload {
            let outcome = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("reload worker disconnected")))
                }
                Err(TryRecvError::Empty) if Instant::now() >= *until => {
                    Some(Err(anyhow::anyhow!("reload preparation timed out")))
                }
                Err(TryRecvError::Empty) => None,
            };
            if let Some(result) = outcome {
                reload = None;
                // A pulse may have started while asynchronous preparation ran.
                if matches!(schedule.phase, Phase::Depulsing(_)) {
                    controller.depulse_end()?;
                    schedule.phase = Phase::Sampling(Instant::now());
                }
                match result.and_then(|next| controller.commit_reload(next)) {
                    Ok(()) => {
                        awaiting_initial = Some(
                            Instant::now() + controller.sensors.initial_timeout(options.interval),
                        );
                        tracing::info!("configuration reloaded");
                        if !matches!(schedule.phase, Phase::Paused) {
                            schedule.phase = Phase::Sampling(Instant::now());
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %format_args!("{error:#}"), "reload rejected; keeping previous configuration")
                    }
                }
            }
        }
        if signals.dump {
            controller.log_snapshot("temperature snapshot");
            workers.log_snapshot();
        }
        let (fan_fault, fan_changed) = if matches!(schedule.phase, Phase::Paused) {
            (false, false)
        } else {
            match controller
                .fans
                .refresh_async(workers, Instant::now(), options.interval)
            {
                Ok(state) => state,
                Err(error) => {
                    controller.policy.force_emergency(DecisionReason::FanFault);
                    controller.emergency_all();
                    if wake_deadline.is_some_and(|d| Instant::now() < d) {
                        tracing::warn!(error = %format_args!("{error:#}"), "waiting for fan hardware after resume");
                        (true, true)
                    } else {
                        return Err(error);
                    }
                }
            }
        };
        let acquisition = controller
            .sensors
            .drain(options.interval, controller.clock.elapsed());
        let changed = match acquisition {
            Ok(changed) => changed,
            Err(error) => {
                controller
                    .policy
                    .force_emergency(DecisionReason::SensorFault);
                controller.emergency_all();
                if wake_deadline.is_some_and(|d| Instant::now() < d) {
                    false
                } else {
                    return Err(error);
                }
            }
        };
        // Capacity release can wake us without a new usable sample (old epoch,
        // preparation or probe completion). Retry due work immediately.
        if signals.work_ready {
            schedule.work_ready(Instant::now());
        }
        if (changed || fan_changed) && !matches!(schedule.phase, Phase::Paused) {
            if matches!(schedule.phase, Phase::Depulsing(_)) {
                controller.depulse_end()?;
            }
            schedule.phase = Phase::Sampling(Instant::now());
        }
        // A signal event is not a timer expiration. Preserve absolute deadlines
        // even across repeated diagnostics, failed reloads or spurious wakeups.
        match schedule.phase {
            Phase::Paused => continue,
            Phase::Sampling(at) | Phase::Depulsing(at) if Instant::now() < at => continue,
            Phase::Depulsing(_) => {
                controller.depulse_end()?;
                schedule.phase =
                    Phase::Sampling(schedule.after_depulse.take().unwrap_or_else(Instant::now));
            }
            Phase::Sampling(deadline) => {
                tracing::debug!(
                    lateness_us = Instant::now()
                        .saturating_duration_since(deadline)
                        .as_micros() as u64,
                    "sampling deadline"
                );
                controller.sensors.schedule(workers, controller.clock)?;
                if let Err(error) = controller
                    .sensors
                    .check_timeouts(controller.clock.elapsed(), options.interval)
                {
                    controller
                        .policy
                        .force_emergency(DecisionReason::SensorFault);
                    controller.emergency_all();
                    if wake_deadline.is_none_or(|d| Instant::now() >= d) {
                        return Err(error);
                    }
                }
                if awaiting_initial.is_some()
                    && controller.sensors.samples.iter().any(|s| s.sequence > 0)
                {
                    awaiting_initial = None;
                }
                if awaiting_initial.is_some_and(|until| Instant::now() < until) {
                    let delay = controller.sensors.async_delay(
                        controller.clock.elapsed(),
                        options.interval,
                        options.interval,
                    );
                    schedule.phase =
                        Phase::Sampling((Instant::now() + delay).min(awaiting_initial.unwrap()));
                    continue;
                }
                let now = controller.clock.elapsed();
                inputs.clear();
                inputs.extend(
                    controller
                        .sensors
                        .samples
                        .iter()
                        .map(|s| (s.sequence, s.generation, s.availability(now))),
                );
                let faults = (controller.sensors.uncertain, fan_fault);
                let maintenance_due = Instant::now() >= schedule.next_maintenance;
                let needs_control = changed
                    || fan_changed
                    || controller.speeds.is_empty()
                    || inputs != last_inputs
                    || faults != last_faults
                    || maintenance_due;
                let tick_ok = if !needs_control {
                    false
                } else {
                    match controller.apply_samples(None, Some(fan_fault), maintenance_due) {
                        Ok(()) => true,
                        Err(error) => {
                            controller.emergency_all();
                            if wake_deadline.is_some_and(|d| Instant::now() < d) {
                                tracing::warn!(error = %format_args!("{error:#}"), "waiting for hardware after resume");
                                false
                            } else {
                                return Err(error);
                            }
                        }
                    }
                };
                if needs_control {
                    last_inputs.clone_from(&inputs);
                    last_faults = faults;
                    // Completion events do not postpone periodic mode/watchdog maintenance.
                    if maintenance_due {
                        schedule.next_maintenance = Instant::now() + options.interval;
                    }
                }
                if once {
                    if controller.sensors.initialized() {
                        return controller.restore();
                    }
                    ensure!(
                        Instant::now() < once_deadline,
                        "one-shot sensor acquisition timed out"
                    );
                }
                controller.next_interval = controller.sensors.async_delay(
                    controller.clock.elapsed(),
                    controller.adaptive.interval(),
                    options.interval,
                );
                let sample_at =
                    (Instant::now() + controller.next_interval).min(schedule.next_maintenance);
                controller.next_interval = sample_at.saturating_duration_since(Instant::now());
                if tick_ok
                    && !controller.emergency_active()
                    && !options.depulse.is_zero()
                    && options.depulse < controller.next_interval
                {
                    controller.depulse_start()?;
                    schedule.after_depulse = Some(sample_at);
                    schedule.phase = Phase::Depulsing(Instant::now() + options.depulse);
                } else {
                    schedule.phase = Phase::Sampling(sample_at);
                }
            }
        }
    }
}
