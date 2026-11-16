//! Sealed forms on the server: a relay that keeps forms in memory only.
//!
//! The server never sees a value. It holds each form's signed spec (purpose,
//! field names and types, target, expiry: not secret) and, between a
//! channel's reply and the dispatcher's delivery, the sealed reply
//! (ciphertext). Nothing about a form is written to the database except a
//! task event naming it; the sealed reply is dropped once the dispatcher
//! answers, and a restart forgets open forms (their links stop working).
//! Crypto and targets: `sealed.rs`.
use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::sealed::{self, Sealed, Signed, Spec};

pub const OPEN: &str = "open";
/// A reply arrived; the dispatcher has not taken it yet.
pub const SUBMITTED: &str = "submitted";
pub const DELIVERED: &str = "delivered";
pub const EXPIRED: &str = "expired";
/// How long a finished form stays visible to `axe form show`.
const KEEP_FINISHED: i64 = 3600;
/// How long after a form finishes the server closes its prompt itself, so a
/// channel that took the reply can close it first (with its outcome reply).
const CLOSE_AFTER: i64 = 2;

/// Where a form's Telegram prompt went, so the reply can be answered there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub bot: String,
    pub chat: i64,
}

#[derive(Debug, Clone)]
pub struct Form {
    pub signed: Signed,
    pub spec: Spec,
    pub expires: i64,
    pub created: i64,
    pub state: &'static str,
    /// The sealed reply, held only while `submitted`.
    sealed: Option<String>,
    /// Where the dispatcher wrote the values (never the values).
    pub delivered: Option<Value>,
    /// The last rejected reply's reason (the form stays open).
    pub error: Option<String>,
    pub prompt: Option<Prompt>,
    pub finished: Option<i64>,
    /// The prompt's keyboard was dealt with once the form finished.
    closed: bool,
}

impl Form {
    pub fn view(&self, page: &str) -> Value {
        let fields: Vec<Value> = self
            .spec
            .fields
            .iter()
            .map(|f| {
                let mut v = json!({"name": f.name, "type": f.kind.as_str()});
                if f.required {
                    v["required"] = json!(true);
                }
                if let Some(e) = &f.env {
                    v["env"] = json!(e);
                }
                v
            })
            .collect();
        json!({
            "form": self.spec.form,
            "state": self.state,
            "purpose": self.spec.purpose,
            "from": self.spec.from,
            "target": self.spec.target,
            "fields": fields,
            "expires": self.spec.expires,
            "fingerprint": sealed::fingerprint_b64(&self.spec.pubkey).unwrap_or_default(),
            "link": sealed::link(page, &self.signed),
            "delivered": self.delivered,
            "error": self.error,
        })
    }
}

/// The dispatcher's side of a delivery, as the relay reports it.
pub enum Delivery {
    /// `{"delivered": ...}` from the dispatcher.
    Done(Value),
    /// The dispatcher refused the reply (`{"error": ...}`).
    Refused(String),
    /// No dispatcher answered; the reply is kept for a retry.
    Unavailable(String),
}

#[derive(Default)]
pub struct Forms {
    forms: Mutex<HashMap<String, Form>>,
    cv: Condvar,
}

impl Forms {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Form>> {
        self.forms.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a form the dispatcher signed for `req`. The spec must match
    /// the request: a dispatcher cannot change the target or the fields.
    pub fn add(&self, req: &sealed::Request, signed: Signed, now: i64) -> Result<Form> {
        let spec = signed.decode()?;
        if spec.target != req.target || spec.fields != req.fields || spec.v != sealed::VERSION {
            bail!("the dispatcher signed a different form");
        }
        if !sealed::is_form_id(&spec.form) {
            bail!("the dispatcher returned an invalid form id");
        }
        let expires = spec.expires_at()?;
        let form = Form {
            signed,
            spec,
            expires,
            created: now,
            state: OPEN,
            sealed: None,
            delivered: None,
            error: None,
            prompt: None,
            finished: None,
            closed: false,
        };
        self.lock().insert(form.spec.form.clone(), form.clone());
        Ok(form)
    }

    pub fn set_prompt(&self, form: &str, prompt: Prompt) {
        if let Some(f) = self.lock().get_mut(form) {
            f.prompt = Some(prompt);
        }
    }

    pub fn get(&self, form: &str, now: i64) -> Option<Form> {
        self.sweep(now);
        self.lock().get(form).cloned()
    }

    pub fn list(&self, now: i64) -> Vec<Form> {
        self.sweep(now);
        let mut v: Vec<Form> = self.lock().values().cloned().collect();
        v.sort_by_key(|f| (f.created, f.spec.form.clone()));
        v
    }

    /// Expires open forms past their time (dropping any held reply) and
    /// forgets finished ones after [`KEEP_FINISHED`].
    pub fn sweep(&self, now: i64) {
        let mut forms = self.lock();
        let mut changed = false;
        for f in forms.values_mut() {
            if matches!(f.state, OPEN | SUBMITTED) && now > f.expires {
                f.state = EXPIRED;
                f.sealed = None;
                f.finished = Some(now);
                changed = true;
            }
        }
        forms.retain(|_, f| f.finished.is_none_or(|t| now - t < KEEP_FINISHED));
        if changed {
            self.cv.notify_all();
        }
    }

    /// Takes a channel's sealed reply for its form; the form must be open.
    /// Returns the form to deliver.
    pub fn submit(&self, text: &str, now: i64) -> Result<Form> {
        let sealed = Sealed::parse(text)?;
        self.sweep(now);
        let mut forms = self.lock();
        let Some(f) = forms.get_mut(&sealed.form) else {
            bail!("unknown or expired form");
        };
        match f.state {
            OPEN => {}
            SUBMITTED => bail!("a reply to this form is already being delivered"),
            DELIVERED => bail!("this form was already used"),
            _ => bail!("this form expired"),
        }
        f.state = SUBMITTED;
        f.sealed = Some(sealed.text());
        f.error = None;
        Ok(f.clone())
    }

    /// The relay job that delivers a submitted form, if it still is one.
    pub fn job(&self, form: &str) -> Option<Value> {
        let forms = self.lock();
        let f = forms.get(form)?;
        let sealed = f.sealed.as_ref().filter(|_| f.state == SUBMITTED)?;
        Some(json!({"kind": "form_deliver", "signed": f.signed, "sealed": sealed}))
    }

    /// Records the dispatcher's answer. The sealed reply is dropped unless
    /// no dispatcher answered.
    pub fn finish(&self, form: &str, d: &Delivery, now: i64) -> Option<Form> {
        let mut forms = self.lock();
        let f = forms.get_mut(form)?;
        if f.state != SUBMITTED {
            return Some(f.clone());
        }
        match d {
            Delivery::Done(v) => {
                f.state = DELIVERED;
                f.sealed = None;
                f.delivered = Some(v.clone());
                f.finished = Some(now);
            }
            Delivery::Refused(e) => {
                f.sealed = None;
                f.error = Some(e.clone());
                // Used or expired on the dispatcher: final. Otherwise the
                // reply was bad and the form stays open for another one.
                if e.contains("already used") || e.contains("expired") {
                    f.state = EXPIRED;
                    f.finished = Some(now);
                } else {
                    f.state = OPEN;
                }
            }
            Delivery::Unavailable(e) => f.error = Some(e.clone()),
        }
        self.cv.notify_all();
        Some(f.clone())
    }

    /// Submitted forms still waiting for a dispatcher.
    pub fn undelivered(&self) -> Vec<String> {
        self.lock()
            .values()
            .filter(|f| f.state == SUBMITTED)
            .map(|f| f.spec.form.clone())
            .collect()
    }

    /// Claims closing a finished form's prompt for a reply that came back in
    /// `at` (its prompt's bot and chat). `Some(remove)` when the caller closes
    /// it: `remove` is false while another open form's keyboard is in that
    /// chat. `None` when the form is not finished, was prompted elsewhere or
    /// is already closed.
    pub fn claim_close(&self, form: &str, at: &Prompt) -> Option<bool> {
        let mut forms = self.lock();
        let f = forms.get_mut(form)?;
        if f.finished.is_none() || f.closed || f.prompt.as_ref() != Some(at) {
            return None;
        }
        f.closed = true;
        Some(!open_prompt_in(&forms, at))
    }

    /// Finished forms whose prompt nobody closed yet (expired, rejected for
    /// good, or delivered through the browser or `axe form submit`), each
    /// with whether to remove the keyboard. Marks them closed. Forms asked
    /// without a prompt (`--no-telegram`) are never returned.
    pub fn take_closing(&self, now: i64) -> Vec<(Form, bool)> {
        let mut forms = self.lock();
        let mut out = vec![];
        for f in forms.values_mut() {
            let due = f.finished.is_some_and(|t| now - t >= CLOSE_AFTER);
            if due && !f.closed && f.prompt.is_some() {
                f.closed = true;
                out.push(f.clone());
            }
        }
        out.into_iter()
            .map(|f| {
                let remove = !open_prompt_in(&forms, f.prompt.as_ref().unwrap());
                (f, remove)
            })
            .collect()
    }

    /// Waits up to `timeout` for the form to leave `open`/`submitted`.
    pub fn wait(&self, form: &str, timeout: Duration, now: impl Fn() -> i64) -> Option<Form> {
        let deadline = Instant::now() + timeout;
        let mut forms = self.lock();
        loop {
            let f = forms.get(form)?;
            if !matches!(f.state, OPEN | SUBMITTED) {
                return Some(f.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Some(f.clone());
            }
            if now() > f.expires {
                drop(forms);
                self.sweep(now());
                forms = self.lock();
                continue;
            }
            let step = left.min(Duration::from_secs(1));
            forms = self
                .cv
                .wait_timeout(forms, step)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Whether an open form's prompt keyboard is in `at`'s chat: removing the
/// keyboard there would hide that form's button.
fn open_prompt_in(forms: &HashMap<String, Form>, at: &Prompt) -> bool {
    forms
        .values()
        .any(|f| matches!(f.state, OPEN | SUBMITTED) && f.prompt.as_ref() == Some(at))
}

/// The owner-facing prompt text for a form (no values; names only).
pub fn prompt_text(f: &Form) -> String {
    let fields: Vec<String> = f
        .spec
        .fields
        .iter()
        .map(|x| {
            let label = if x.label.is_empty() {
                &x.name
            } else {
                &x.label
            };
            format!("{label} ({})", x.kind.as_str())
        })
        .collect();
    let from = if f.spec.from.is_empty() {
        String::new()
    } else {
        format!(" from {}", f.spec.from)
    };
    format!(
        "Sealed form{from}: {}\nFor: {}\nFields: {}\nDispatcher key: {}\nExpires: {}\n\n\
         Tap \"Open sealed form\" below. The values are encrypted on your phone to the \
         dispatcher key; Telegram and the axe server see only ciphertext.",
        f.spec.purpose,
        f.spec.target.describe(),
        fields.join(", "),
        sealed::fingerprint_b64(&f.spec.pubkey).unwrap_or_default(),
        f.spec.expires,
    )
}

/// The owner-facing outcome of a reply.
pub fn outcome_text(f: &Form) -> String {
    match f.state {
        DELIVERED => {
            let files = f.delivered.as_ref().map(|d| {
                d["files"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default()
            });
            match files.filter(|s| !s.is_empty()) {
                Some(files) => format!(
                    "Sealed form delivered to {}: {files}.",
                    f.spec.target.describe()
                ),
                None => format!("Sealed form delivered: {}.", f.spec.target.describe()),
            }
        }
        SUBMITTED => "Sealed form received; the dispatcher is offline, so it is delivered when it reconnects.".into(),
        OPEN => format!(
            "Sealed form reply refused: {}. The form is still open.",
            f.error.as_deref().unwrap_or("unknown error")
        ),
        _ => format!(
            "Sealed form not delivered: {}.",
            f.error.as_deref().unwrap_or("it expired")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::{Field, Request, Target};

    fn req() -> Request {
        Request {
            purpose: "Token".into(),
            target: Target::Task { task: 3 },
            fields: vec![Field::parse("token:secret:required").unwrap()],
            ttl: Some(120),
            from: "task 3".into(),
        }
    }

    #[test]
    fn signed_spec_must_match_the_request() {
        let forms = Forms::default();
        let key = crate::sealed::Key::random();
        let (_, s) = key.sign("d", &req(), 0).unwrap();
        let mut other = req();
        other.target = Target::Task { task: 4 };
        assert!(forms.add(&other, s.clone(), 0).is_err());
        assert!(forms.add(&req(), s, 0).is_ok());
    }

    #[test]
    fn lifecycle_holds_ciphertext_only_until_delivery() {
        let forms = Forms::default();
        let key = crate::sealed::Key::random();
        let (spec, s) = key.sign("d", &req(), 0).unwrap();
        let f = forms.add(&req(), s, 0).unwrap();
        let sealed = crate::sealed::seal(&spec, &json!({"token": "v"})).unwrap();
        assert!(forms.submit("garbage", 1).is_err());
        forms.submit(&sealed.text(), 1).unwrap();
        assert!(
            forms.submit(&sealed.text(), 1).is_err(),
            "one reply at a time"
        );
        assert!(forms.job(&f.spec.form).is_some());
        // No dispatcher: kept for a retry.
        forms.finish(&f.spec.form, &Delivery::Unavailable("offline".into()), 2);
        assert!(forms.job(&f.spec.form).is_some());
        // A bad reply reopens the form and drops the ciphertext.
        forms.finish(
            &f.spec.form,
            &Delivery::Refused("does not decrypt".into()),
            2,
        );
        assert_eq!(forms.get(&f.spec.form, 2).unwrap().state, OPEN);
        assert!(forms.job(&f.spec.form).is_none());
        forms.submit(&sealed.text(), 3).unwrap();
        let done = forms
            .finish(
                &f.spec.form,
                &Delivery::Done(json!({"files": ["token"]})),
                3,
            )
            .unwrap();
        assert_eq!(done.state, DELIVERED);
        assert!(done.sealed.is_none());
        assert!(forms
            .submit(&sealed.text(), 4)
            .unwrap_err()
            .to_string()
            .contains("already used"));
        assert_eq!(
            outcome_text(&done),
            "Sealed form delivered to task 3: token."
        );
        // Finished forms are forgotten after a while.
        forms.sweep(3 + KEEP_FINISHED);
        assert!(forms.get(&f.spec.form, 3 + KEEP_FINISHED).is_none());
    }

    #[test]
    fn expiry_drops_a_held_reply() {
        let forms = Forms::default();
        let key = crate::sealed::Key::random();
        let (spec, s) = key.sign("d", &req(), 0).unwrap();
        forms.add(&req(), s, 0).unwrap();
        let sealed = crate::sealed::seal(&spec, &json!({"token": "v"})).unwrap();
        forms.submit(&sealed.text(), 1).unwrap();
        forms.sweep(121);
        let f = forms.get(&spec.form, 121).unwrap();
        assert_eq!(f.state, EXPIRED);
        assert!(f.sealed.is_none());
        assert!(forms.job(&spec.form).is_none());
        assert!(forms.submit(&sealed.text(), 122).is_err());
    }

    #[test]
    fn finished_prompts_close_once() {
        let forms = Forms::default();
        let key = crate::sealed::Key::random();
        let at = Prompt {
            bot: "cos".into(),
            chat: 7,
        };
        let mut ids = vec![];
        for _ in 0..3 {
            let (_, s) = key.sign("d", &req(), 0).unwrap();
            let f = forms.add(&req(), s, 0).unwrap();
            forms.set_prompt(&f.spec.form, at.clone());
            ids.push(f.spec.form);
        }
        // No prompt (--no-telegram): never closed.
        let (_, s) = key.sign("d", &req(), 0).unwrap();
        forms.add(&req(), s, 0).unwrap();
        assert!(forms.take_closing(100).is_empty(), "all open");

        // Delivered through the chat: the channel claims it once; the other
        // open forms keep the keyboard.
        let spec = forms.get(&ids[0], 1).unwrap().spec;
        let sealed = crate::sealed::seal(&spec, &json!({"token": "v"})).unwrap();
        forms.submit(&sealed.text(), 1).unwrap();
        assert_eq!(forms.claim_close(&ids[0], &at), None, "not finished");
        forms.finish(&ids[0], &Delivery::Done(json!({"files": []})), 1);
        let elsewhere = Prompt {
            bot: "cos".into(),
            chat: 8,
        };
        assert_eq!(forms.claim_close(&ids[0], &elsewhere), None);
        assert_eq!(forms.claim_close(&ids[0], &at), Some(false));
        assert_eq!(forms.claim_close(&ids[0], &at), None, "once");
        assert!(forms.take_closing(100).is_empty());

        // Expiry: the server closes them, removing the keyboard only once no
        // open form is left in the chat; not before CLOSE_AFTER.
        forms.sweep(121);
        assert!(forms.take_closing(121).is_empty(), "too soon");
        let mut closing = forms.take_closing(121 + CLOSE_AFTER);
        closing.sort_by_key(|(f, _)| f.spec.form.clone());
        assert_eq!(closing.len(), 2);
        assert!(closing
            .iter()
            .all(|(f, remove)| f.state == EXPIRED && *remove));
        assert!(forms.take_closing(200).is_empty(), "once");
    }

    #[test]
    fn prompt_names_fields_not_values() {
        let key = crate::sealed::Key::random();
        let (_, s) = key.sign("d", &req(), 0).unwrap();
        let f = Forms::default().add(&req(), s, 0).unwrap();
        let t = prompt_text(&f);
        assert!(t.contains("token (secret)"));
        assert!(t.contains(&key.fingerprint()));
        assert!(t.contains("For: task 3"));
    }
}
