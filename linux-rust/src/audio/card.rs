//! Bluetooth card profile control and stream routing, shared by the media
//! controller (ear detection, ownership) and the hi-res mic (A2DP reset).
//!
//! Every profile change goes through PROFILE_LOCK. Before, activate (ear in),
//! deactivate (ears out / ownership lost) and the hi-res A2DP reset (off -> on)
//! ran concurrently from different tasks and could interleave, e.g. leaving the
//! card on with the buds out or off with them in.

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::operation::State as OperationState;
use log::{debug, info, warn};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::audio::output::connect;

static PROFILE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    PROFILE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Copy, Debug)]
pub enum CardRef<'a> {
    Index(u32),
    Name(&'a str),
}

/// Active profile name of a card ("off", "a2dp-sink", ...), or None if the card
/// doesn't exist (yet).
pub fn active_profile(card: CardRef) -> Option<String> {
    let (mut mainloop, context) = connect()?;
    let introspect = context.introspect();
    let profile: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let cb = {
        let profile = profile.clone();
        move |result: ListResult<&libpulse_binding::context::introspect::CardInfo>| {
            if let ListResult::Item(item) = result {
                *profile.borrow_mut() = item
                    .active_profile
                    .as_ref()
                    .and_then(|p| p.name.as_ref())
                    .map(|n| n.to_string());
            }
        }
    };
    let op = match card {
        CardRef::Index(i) => introspect.get_card_info_by_index(i, cb),
        CardRef::Name(n) => introspect.get_card_info_by_name(n, cb),
    };
    while op.get_state() == OperationState::Running {
        mainloop.iterate(true);
    }
    drop(op);
    let result = profile.borrow().clone();
    result
}

fn set_profile_unlocked(card: CardRef, profile: &str) -> bool {
    let Some((mut mainloop, context)) = connect() else {
        return false;
    };
    let mut introspect = context.introspect();
    let ok = Rc::new(Cell::new(false));
    let cb: Box<dyn FnMut(bool) + 'static> = {
        let ok = ok.clone();
        Box::new(move |success| ok.set(success))
    };
    let op = match card {
        CardRef::Index(i) => introspect.set_card_profile_by_index(i, profile, Some(cb)),
        CardRef::Name(n) => introspect.set_card_profile_by_name(n, profile, Some(cb)),
    };
    while op.get_state() == OperationState::Running {
        mainloop.iterate(true);
    }
    let success = ok.get();
    if !success {
        warn!("[pw] setting profile {} on {:?} failed", profile, card);
    }
    success
}

/// Set a card profile, serialized with every other profile change. Returns the
/// real result reported by the server.
pub fn set_profile(card: CardRef, profile: &str) -> bool {
    let _guard = lock();
    set_profile_unlocked(card, profile)
}

/// Cycle an A2DP card off and back on (fixes the transport after a hi-res mic
/// START/STOP). If the card isn't on an A2DP profile yet (buds just inserted,
/// profile still being activated) wait up to `wait` for it rather than skipping
/// the reset or doing a useless "off -> off". Returns true if a cycle happened.
pub fn cycle_a2dp(card: &str, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        // Not holding the lock while waiting: activation needs it to finish.
        let current = active_profile(CardRef::Name(card));
        if current.as_deref().is_some_and(|p| p.starts_with("a2dp")) {
            break;
        }
        if Instant::now() >= deadline {
            info!(
                "[pw] {} not on an A2DP profile ({:?}); skipping A2DP reset",
                card, current
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let _guard = lock();
    // Re-read under the lock: a deactivation may have won the race meanwhile.
    let Some(profile) = active_profile(CardRef::Name(card)).filter(|p| p.starts_with("a2dp"))
    else {
        debug!("[pw] {} left A2DP before the reset; skipping", card);
        return false;
    };
    info!("[pw] reset A2DP transport: {} off -> {}", card, profile);
    let off = set_profile_unlocked(CardRef::Name(card), "off");
    let on = set_profile_unlocked(CardRef::Name(card), &profile);
    off && on
}

/// Sink indices that belong to a card.
fn sinks_of_card(card_index: u32) -> Vec<u32> {
    let Some((mut mainloop, context)) = connect() else {
        return Vec::new();
    };
    let introspect = context.introspect();
    let sinks: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));
    let op = introspect.get_sink_info_list({
        let sinks = sinks.clone();
        move |result| {
            if let ListResult::Item(item) = result
                && item.card == Some(card_index)
            {
                sinks.borrow_mut().push(item.index);
            }
        }
    });
    while op.get_state() == OperationState::Running {
        mainloop.iterate(true);
    }
    drop(op);
    let result = sinks.borrow().clone();
    result
}

/// Playback streams (sink-input indices) currently playing to a card's sinks.
/// Recorded before switching the card off so they can be moved back later.
pub fn streams_on_card(card_index: u32) -> Vec<u32> {
    let sinks = sinks_of_card(card_index);
    if sinks.is_empty() {
        return Vec::new();
    }
    let Some((mut mainloop, context)) = connect() else {
        return Vec::new();
    };
    let introspect = context.introspect();
    let streams: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));
    let op = introspect.get_sink_input_info_list({
        let streams = streams.clone();
        move |result| {
            if let ListResult::Item(item) = result
                && sinks.contains(&item.sink)
            {
                streams.borrow_mut().push(item.index);
            }
        }
    });
    while op.get_state() == OperationState::Running {
        mainloop.iterate(true);
    }
    drop(op);
    let result = streams.borrow().clone();
    result
}

/// Move the given playback streams (those that still exist) to the card's sink.
/// Waits up to `wait` for the sink to appear after the profile was set.
pub fn move_streams_to_card(streams: &[u32], card_index: u32, wait: Duration) {
    if streams.is_empty() {
        return;
    }
    let deadline = Instant::now() + wait;
    let sink = loop {
        if let Some(&s) = sinks_of_card(card_index).first() {
            break s;
        }
        if Instant::now() >= deadline {
            warn!("[pw] no sink for card {} yet; not moving streams back", card_index);
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let Some((mut mainloop, context)) = connect() else {
        return;
    };
    let mut introspect = context.introspect();
    let mut moved = 0;
    for &stream in streams {
        let ok = Rc::new(Cell::new(false));
        let cb: Box<dyn FnMut(bool) + 'static> = {
            let ok = ok.clone();
            Box::new(move |success| ok.set(success))
        };
        let op = introspect.move_sink_input_by_index(stream, sink, Some(cb));
        while op.get_state() == OperationState::Running {
            mainloop.iterate(true);
        }
        // A stream that ended meanwhile just fails to move; that's fine.
        if ok.get() {
            moved += 1;
        }
    }
    info!("[pw] moved {} of {} stream(s) back to the AirPods", moved, streams.len());
}
