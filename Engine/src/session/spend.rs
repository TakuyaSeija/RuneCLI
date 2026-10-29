//! What a session has cost, said whole and in order.
//!
//! The sidebar's spend is the engine's answer, never a sum a window keeps: a
//! request that ran before its model had a price is priced later, when the
//! price is filled in (owner, 2026-10-23), and only the store holds every
//! request to price. So the total is worked out from the store and sent as
//! [`Event::Spend`] after each recorded request and whenever prices may have
//! moved; a window shows the latest one.
//!
//! Two answers worked out on different threads must arrive in the order the
//! store moved, or a total from before a request would land after the total
//! that counts it. Working one out and sending it is therefore one step under
//! [`IN_ORDER`]; the settings and the store are each taken and let go inside
//! it, never one inside the other.

use std::sync::Mutex;

use super::Runtime;
use crate::panic;
use crate::proto::Event;

static IN_ORDER: Mutex<()> = Mutex::new(());

/// Send `session`'s cost as it stands now to every window of it.
pub fn publish(rt: &Runtime, session: i64) {
    let _order = panic::lock(&IN_ORDER);
    let prices = panic::lock(&rt.settings).prices();
    let usage = panic::lock(&rt.db).session_usage(session, &prices);
    // A store that does not answer has already been said by the write that
    // failed; the window keeps the total it had.
    if let Ok(usage) = usage {
        let _ = rt.events.send(Event::Spend {
            session,
            spend: usage.spend,
            priced: usage.priced_calls,
            unpriced: usage.unpriced_calls,
        });
    }
}
