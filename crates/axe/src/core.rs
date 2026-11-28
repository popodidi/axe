//! The core every front end calls: one action per change or query, so the
//! HTTP API, the Telegram gateway and the node do the same thing the same
//! way (store doc harvis/docs/decisions/axe-layering-review.md). The server
//! implements [`Axe`] for its state and injects it into each channel.
//!
//! An action validates, writes through the primitive's storage module and
//! notifies ([`Axe::changed`]) itself; a query reads. Channel code calls
//! only these methods: never [`Axe::db`] or [`Axe::changed`], and no SQL
//! (`tests/layering.rs` enforces it).
use std::sync::MutexGuard;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

use crate::ask;
use crate::board;
use crate::bot::{self, NewMessage};
use crate::channel::{self, StoredMessage};
use crate::db::Actor;
use crate::goal_board;
use crate::owner_views;

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// The read-only owner views (`/status`, `/tasks`, ...), served by
/// Telegram and `GET /api/views/<view>`.
pub const VIEWS: [&str; 8] = [
    "status", "node", "tasks", "task", "cost", "goals", "services", "routines",
];

/// What every front end calls; the server implements it.
pub trait Axe: Send + Sync + 'static {
    /// The database, for the actions below only: channel code never calls
    /// it (`tests/layering.rs`).
    fn db(&self) -> MutexGuard<'_, Connection>;
    /// A database change was committed (wakes SSE clients and the board
    /// loops). Called by the actions below, not by channel code.
    fn changed(&self);
    /// The change counter that [`Axe::changed`] bumps.
    fn seq(&self) -> u64;
    /// Waits until the counter moves past `seen` or `timeout` elapses; returns
    /// the counter.
    fn wait_changed(&self, seen: u64, timeout: std::time::Duration) -> u64;
    fn stopping(&self) -> bool;
    /// The node's runtimes (`runtime::view`); none outside a server.
    fn runtimes(&self) -> Value {
        Value::Null
    }
    /// Takes a channel's sealed form reply and delivers it to the node
    /// (`form.rs`); returns the form after the attempt.
    fn form_submit(&self, _sealed: &str) -> Result<crate::form::Form> {
        anyhow::bail!("sealed forms need an axe server")
    }
    /// Claims closing a finished form's prompt from the reply's `bot` and
    /// `chat` (`form::Forms::claim_close`): `Some(open)` when the caller
    /// re-sends the keyboard with the `open` forms' buttons (purpose, link;
    /// none: removes it), `None` when it does nothing.
    fn form_close(&self, _form: &str, _bot: &str, _chat: i64) -> Option<Vec<(String, String)>> {
        None
    }
    /// Starts a login of a node runtime by `method`
    /// (`runtime::login_methods`) on behalf of the owner, prompted through
    /// `bot`; returns the text for the owner.
    fn runtime_login(&self, _bot: &str, _runtime: &str, _method: &str) -> Result<String> {
        anyhow::bail!("runtime logins need an axe server")
    }
    /// Adds a node runtime of `harness` (`runtime::Registry::create`); a
    /// `bot` with no runtime yet gets it as its primary. Returns its name.
    fn runtime_add(&self, _actor: &Actor, _harness: &str, _bot: Option<&str>) -> Result<String> {
        anyhow::bail!("runtime changes need an axe server")
    }
    /// Whether a bot may run on `name` (a primary or backup), the check
    /// every bot edit makes ([`crate::bot::edit`]).
    fn bot_runtime_ok(&self, _name: &str) -> Result<()> {
        anyhow::bail!("runtime checks need an axe server")
    }
    /// Removes node runtime `runtime` (`runtime::Registry::remove`);
    /// returns what happened to its login files.
    fn runtime_remove(&self, _runtime: &str) -> Result<String> {
        anyhow::bail!("runtime changes need an axe server")
    }

    // Queries.

    /// Bot `name`'s row id.
    fn bot_id(&self, name: &str) -> Result<i64> {
        channel::bot_id(&self.db(), name)
    }
    fn get_bot(&self, name: &str) -> Result<bot::Bot> {
        bot::get(&self.db(), name)
    }
    /// The axe bot config name `name` stands for (`bot::bound_name`).
    fn bound_name(&self, name: &str) -> Result<Option<String>> {
        bot::bound_name(&self.db(), name)
    }
    /// The id of bot `bot_id`'s latest event, 0 with none.
    fn last_bot_event(&self, bot_id: i64) -> i64 {
        bot::last_event_id(&self.db(), bot_id)
    }
    /// Bot `bot_id`'s `attention` events after event `after`.
    fn attention_since(&self, bot_id: i64, after: i64) -> Result<Vec<(i64, String)>> {
        bot::attention_since(&self.db(), bot_id, after)
    }
    fn message(&self, bot: &str, id: i64) -> Result<StoredMessage> {
        channel::get_message(&self.db(), bot, id)
    }
    /// The stored message for Telegram message `tg_msg` in `chat`, if any.
    fn message_at(&self, bot: &str, chat: i64, tg_msg: i64) -> Result<Option<StoredMessage>> {
        let conn = self.db();
        let id = channel::bot_id(&conn, bot)?;
        match channel::local_id(&conn, id, chat, tg_msg)? {
            Some(l) => Ok(Some(channel::get_message(&conn, bot, l)?)),
            None => Ok(None),
        }
    }
    /// The chat of bot `bot`'s latest inbound message.
    fn last_inbound_chat(&self, bot: &str) -> Result<Option<i64>> {
        let conn = self.db();
        let id = channel::bot_id(&conn, bot)?;
        channel::last_inbound_chat(&conn, id)
    }
    /// Fails with "task N not found" when task `id` does not exist.
    fn task_exists(&self, id: i64) -> Result<()> {
        crate::db::get_task(&self.db(), id).map(|_| ())
    }
    /// (chat, thread) of the messages whose wakes bot `name`'s live run took.
    fn live_chats(&self, name: &str) -> Result<Vec<(i64, Option<i64>)>> {
        channel::live_chats(&self.db(), name)
    }
    /// Bot `name`'s voice acks due their edit, with their voice messages
    /// (`channel::due_voice_acks`).
    fn due_voice_acks(&self, name: &str) -> Result<Vec<(StoredMessage, StoredMessage)>> {
        channel::due_voice_acks(&self.db(), name)
    }
    /// Bot `name`'s runtime cards that still have buttons, oldest first.
    fn open_runtime_cards(&self, name: &str) -> Result<Vec<StoredMessage>> {
        channel::open_runtime_cards(&self.db(), name)
    }
    /// One read-only owner view ([`VIEWS`]) for bot `bot`, in HTML; `arg`
    /// is the task of `task`, `web` the public URL for links.
    fn view(&self, view: &str, bot: &str, arg: &str, web: Option<&str>) -> Result<String> {
        let conn = self.db();
        match view {
            "status" => owner_views::status(&conn, bot, now()),
            "node" => owner_views::node(&conn, web, now()),
            "tasks" => owner_views::tasks(&conn, bot, web),
            "task" => owner_views::task(&conn, arg, web),
            "cost" => owner_views::cost(&conn, bot, web, now()),
            "goals" => owner_views::goals(&conn, bot, web),
            "services" => owner_views::services(&conn, bot, web, now()),
            "routines" => owner_views::routines(&conn, bot, web, now()),
            v => anyhow::bail!("unknown view {v:?} (expected one of {})", VIEWS.join(", ")),
        }
    }
    fn last_retro(&self, bot: &str) -> Result<Option<i64>> {
        ask::last_retro(&self.db(), bot)
    }
    fn previous_retro(&self, bot: &str, before: i64) -> Result<Option<ask::SentRetro>> {
        ask::previous_retro(&self.db(), bot, before)
    }
    fn ratings(&self, ids: &[String]) -> Result<std::collections::HashMap<String, String>> {
        ask::ratings(&self.db(), ids)
    }
    /// Bot `bot`'s retro card now (`ask::bot_retro`).
    fn bot_retro(&self, bot: &str, limit: Option<usize>) -> Result<ask::Retro> {
        ask::bot_retro(&self.db(), now(), bot, limit)
    }
    fn board_message(&self, bot: &str) -> Result<Option<StoredMessage>> {
        board::message(&self.db(), bot)
    }
    fn goal_board_message(&self, bot: &str) -> Result<Option<StoredMessage>> {
        goal_board::message(&self.db(), bot)
    }
    fn card_needs(&self, bot: &str) -> Result<Vec<board::CardNeed>> {
        board::card_needs(&self.db(), bot)
    }
    fn stale_task_cards(&self, bot: &str) -> Result<Vec<(i64, i64, String)>> {
        board::stale_task_cards(&self.db(), bot)
    }
    fn closed_pins(&self, bot: &str) -> Result<Vec<StoredMessage>> {
        board::closed_pins(&self.db(), bot)
    }
    fn board_lines(&self, bot: &str) -> Result<Vec<board::Line>> {
        board::lines(&self.db(), bot)
    }
    /// Bot `bot`'s active goals and its goal pins.
    fn goal_pins(
        &self,
        bot: &str,
        web: Option<&str>,
    ) -> Result<(Vec<goal_board::GoalLine>, Vec<StoredMessage>)> {
        let conn = self.db();
        Ok((
            goal_board::pinned_lines(&conn, bot, web)?,
            goal_board::pins(&conn, bot)?,
        ))
    }
    fn goal_name(&self, id: i64) -> Result<Option<String>> {
        crate::goal::name_of(&self.db(), id)
    }

    // Actions.

    /// Records bot event `kind` for bot `bot`.
    fn bot_event(&self, bot: &str, actor: &Actor, kind: &str, body: &Value) -> Result<()> {
        {
            let conn = self.db();
            let id = channel::bot_id(&conn, bot)?;
            bot::event(&conn, id, actor, kind, &body.to_string())?;
        }
        self.changed();
        Ok(())
    }
    /// Stores message `m` of bot `bot`; returns its local id.
    fn record_message(&self, bot: &str, m: &NewMessage) -> Result<i64> {
        let (id, _) = bot::record_message(&mut self.db(), bot, m, now())?;
        self.changed();
        Ok(id)
    }
    /// Stores an inbound message `m` of `m.chat_id`, linking its edit and
    /// reply targets by their Telegram ids; returns whether it was stored
    /// before (a redelivered update) and its local id.
    fn record_inbound(
        &self,
        bot: &str,
        edit_of_tg: Option<i64>,
        reply_tg: Option<i64>,
        m: NewMessage,
    ) -> Result<(bool, i64)> {
        let r = {
            let mut conn = self.db();
            let id = channel::bot_id(&conn, bot)?;
            let edit_of = match edit_of_tg {
                Some(t) => channel::local_id(&conn, id, m.chat_id, t)?,
                None => None,
            };
            let reply_to = match reply_tg {
                Some(r) => channel::local_id(&conn, id, m.chat_id, r)?,
                None => None,
            };
            let dup = channel::is_recorded(&conn, id, m.chat_id, m.message_id, &m.kind)?;
            let m = NewMessage {
                edit_of,
                reply_to,
                ..m
            };
            let (msg, _) = bot::record_message(&mut conn, bot, &m, now())?;
            (dup, msg)
        };
        self.changed();
        Ok(r)
    }
    /// A button press `press` (kind `button`, message id the update id) on
    /// Telegram message `tg_msg`: stored and the pressed message marked
    /// answered (with `label` of its flags), unless that keyboard was
    /// already answered by another update. Returns the pressed message and
    /// whether it was already answered.
    fn record_press(
        &self,
        bot: &str,
        tg_msg: Option<i64>,
        press: NewMessage,
        data: &str,
        label: &dyn Fn(&Value) -> String,
    ) -> Result<(Option<StoredMessage>, bool)> {
        let r = {
            let mut conn = self.db();
            let id = channel::bot_id(&conn, bot)?;
            let target = match tg_msg {
                Some(t) => channel::local_id(&conn, id, press.chat_id, t)?,
                None => None,
            };
            let stored = target
                .map(|t| channel::get_message(&conn, bot, t))
                .transpose()?;
            let uid = press.message_id;
            // A later press on an answered keyboard is not a new wake.
            let answered = stored.as_ref().is_some_and(|m| {
                !m.flags["answered"].is_null() && m.flags["answered"]["update"] != uid
            });
            if !answered {
                let press = NewMessage {
                    reply_to: target,
                    ..press
                };
                bot::record_message(&mut conn, bot, &press, now())?;
                if let Some(m) = stored.as_ref() {
                    let answer =
                        serde_json::json!({"label": label(&m.flags), "data": data, "update": uid});
                    channel::mark_answered(&conn, m.id, &answer)?;
                }
            }
            (stored, answered)
        };
        self.changed();
        Ok(r)
    }
    /// A reaction (kind `reaction`) to Telegram message `tg_msg`.
    fn record_reaction(&self, bot: &str, tg_msg: Option<i64>, m: NewMessage) -> Result<()> {
        {
            let mut conn = self.db();
            let id = channel::bot_id(&conn, bot)?;
            let reply_to = match tg_msg {
                Some(t) => channel::local_id(&conn, id, m.chat_id, t)?,
                None => None,
            };
            bot::record_message(&mut conn, bot, &NewMessage { reply_to, ..m }, now())?;
        }
        self.changed();
        Ok(())
    }
    /// [`bot::edit`] with the runtime check every caller makes
    /// ([`Axe::bot_runtime_ok`]).
    fn bot_edit(&self, actor: &Actor, name: &str, e: &bot::Edit, why: &str) -> Result<bot::Bot> {
        let check = |n: &str| self.bot_runtime_ok(n);
        let r = bot::edit(&mut self.db(), actor, name, e, why, &check);
        self.changed();
        r
    }
    /// Stop, pause, resume or reset bot `name` (`bot::change`).
    fn bot_change(&self, actor: &Actor, name: &str, action: &str) -> Result<()> {
        let r = bot::change(&mut self.db(), actor, name, action).map(|_| ());
        self.changed();
        r
    }
    /// A wake asked for (`bot::request_wake`): its id and due time.
    fn request_wake(
        &self,
        actor: &Actor,
        name: &str,
        w: &bot::WakeRequest,
    ) -> Result<(i64, String)> {
        let r = bot::request_wake(&mut self.db(), actor, name, w, now());
        self.changed();
        r
    }
    /// A wake for bot `name` at `due` (`bot::add_wake`); a repeated `key`
    /// returns the existing one and changes nothing.
    fn add_wake(
        &self,
        actor: &Actor,
        name: &str,
        due: i64,
        note: &str,
        key: Option<&str>,
    ) -> Result<(i64, String)> {
        let (r, new) = {
            let mut conn = self.db();
            let new = match key {
                Some(k) => !bot::has_wake(&conn, name, k)?,
                None => true,
            };
            (
                bot::add_wake(&mut conn, actor, name, due, note, key, now())?,
                new,
            )
        };
        if new {
            self.changed();
        }
        Ok(r)
    }
    /// The owner rates ask `id` (`ask::rate`).
    fn rate(&self, actor: &Actor, id: &str, rating: &str) -> Result<ask::Ask> {
        let r = ask::rate(&mut self.db(), actor, id, rating);
        self.changed();
        r
    }
    /// The owner accepts the recommendations of `items` (`ask::accept`).
    fn accept_recs(&self, actor: &Actor, items: &[ask::RetroItem]) -> Result<usize> {
        let r = ask::accept(&mut self.db(), actor, items);
        self.changed();
        r
    }
    /// Sets flag `path` of message `msg` to `v`.
    fn set_flag(&self, msg: i64, path: &str, v: Value) -> Result<()> {
        channel::set_flag(&self.db(), msg, path, &v)?;
        self.changed();
        Ok(())
    }
    /// Marks message `msg` as the runtime card of `runtime`.
    fn mark_runtime_alert(&self, msg: i64, runtime: &str) -> Result<()> {
        channel::mark_runtime_alert(&self.db(), msg, runtime)?;
        self.changed();
        Ok(())
    }
    /// Records an edit of message `msg` (`channel::record_edit`).
    fn record_edit(
        &self,
        msg: i64,
        text: Option<&str>,
        buttons: Option<&[(String, String)]>,
        html: bool,
    ) -> Result<()> {
        channel::record_edit(&self.db(), msg, text, buttons, html)?;
        self.changed();
        Ok(())
    }
    fn record_pin(&self, msg: i64, pin: bool) -> Result<()> {
        channel::record_pin(&self.db(), msg, pin)?;
        self.changed();
        Ok(())
    }
    fn mark_deleted(&self, msg: i64) -> Result<()> {
        channel::mark_deleted(&self.db(), msg)?;
        self.changed();
        Ok(())
    }
    /// Records the server's edit of message `msg` to `text`.
    fn set_text_edited(&self, msg: i64, text: &str) -> Result<()> {
        channel::set_text_edited(&self.db(), msg, text)?;
        self.changed();
        Ok(())
    }
    /// Settles voice ack `msg` (`channel::settle_voice_ack`).
    fn settle_voice_ack(&self, msg: i64, text: Option<&str>) -> Result<()> {
        channel::settle_voice_ack(&self.db(), msg, text)?;
        self.changed();
        Ok(())
    }
    /// Closes card `msg` (`flags.closed` says why).
    fn close_card(&self, msg: i64, reason: &str) -> Result<()> {
        channel::close_card(&self.db(), msg, reason)?;
        self.changed();
        Ok(())
    }
    /// Closes card `msg` with `text` if it still has a keyboard; false when
    /// another pass closed it first.
    fn claim_close(&self, msg: i64, text: &str, reason: &str) -> Result<bool> {
        let n = channel::claim_close(&self.db(), msg, text, reason)?;
        if n > 0 {
            self.changed();
        }
        Ok(n > 0)
    }
    fn mark_card_pinned(&self, msg: i64) -> Result<()> {
        channel::mark_card_pinned(&self.db(), msg)?;
        self.changed();
        Ok(())
    }
    fn clear_board_flags(&self, msg: i64) -> Result<()> {
        channel::clear_board_flags(&self.db(), msg)?;
        self.changed();
        Ok(())
    }
    fn drop_goal_pin(&self, msg: i64, goal_pin: &str) -> Result<()> {
        channel::drop_goal_pin(&self.db(), msg, goal_pin)?;
        self.changed();
        Ok(())
    }
    fn set_goal_pin_text(&self, msg: i64, text: &str) -> Result<()> {
        channel::set_goal_pin_text(&self.db(), msg, text)?;
        self.changed();
        Ok(())
    }
    /// Message `msg` becomes bot `bot`'s board message when it has none
    /// (`board::default_on`).
    fn board_default_on(&self, bot: &str, msg: i64) -> Result<bool> {
        let on = board::default_on(&mut self.db(), bot, msg)?;
        if on {
            self.changed();
        }
        Ok(on)
    }
    /// Bot `bot` has no separate goal board any more.
    fn clear_goal_board(&self, bot: &str) -> Result<()> {
        goal_board::set_message(&mut self.db(), bot, None)?;
        self.changed();
        Ok(())
    }
}
