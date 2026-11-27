---
title: "Sealed forms and messages"
description: "Ask the owner for values only the node can read, and send sealed messages back."
---

Paths in `crates/axe/src/` unless noted.

`axe form ask` (`sealed.rs`, `form.rs`) asks the owner for values (secret,
text, code, choice and file fields) that only the node can read, for
any task or bot session:

```sh
axe form ask --bot ops --purpose "Deploy token" --field token:secret:required:env=DEPLOY_TOKEN --wait
```

- A field can declare a format: `prefix=TEXT`, `len=MIN..MAX` (characters)
  and `chars=digits|nospace`, for example
  `--field chat:code:required:chars=digits` or
  `--field token:secret:prefix=ghp_:chars=nospace`. The page shows the first
  problem under the field ("contains 2 spaces: likely a wrapped line") and
  holds the seal until it is fixed; the node refuses a value that still fails
  with the same words (`Check` in `sealed.rs`). Nothing trims or rewrites a
  value. `/runtimes` login forms declare each method's format
  (`runtime::login_check`), and a runtime login refuses a malformed
  credential by name before storing it.
- `--bot` names the Telegram bot that sends the owner the prompt; it must be
  able to write to the owner's private chat. It is required unless
  `--no-telegram`, which only prints the link. `/runtimes` login forms come
  from the bot the owner typed `/runtimes` in.
- The prompt's "Open sealed form" button is a reply keyboard. A chat shows
  only its latest one, so while several forms are open in a chat the
  keyboard holds one button per form, labelled with its purpose. Once a form
  is delivered, rejected for good or expired, the bot tells the owner the
  outcome in that chat with a keyboard of the forms still open there, or
  removes it (`form_keyboard` and `form_reply` in `telegram.rs`,
  `Forms::take_closing` in `form.rs`).
- To test forms (CI, post-deploy checks), do not prompt the owner: use
  `axe form ask --no-telegram`, then submit a reply sealed to the printed
  link's spec with `axe form submit` (or through the link's page). No
  message reaches Telegram; the test in `crates/axe/tests/telegram.rs`
  (`a_finished_form_removes_its_button_and_no_telegram_forms_send_nothing`)
  pins this.

- The node signs the form with its key (`dispatcher.key`, P-256, 0600,
  made on first start next to the config file; `axe form key` prints its
  fingerprint, as do `axe runtimes`, `/runtimes` and every form). Compare it
  once.
- The link opens a static page (`[forms] page_url`, default
  [`DEFAULT_FORM_PAGE`](../../../crates/axe/src/config.rs)) with the signed spec in its
  URL fragment. The page encrypts the values in the browser to the node
  key and returns `sealed.v1.<form>.<ciphertext>`: through the Telegram Mini
  App button the server sends the owner, pasted into the chat, a `.sealed`
  file, or `axe form submit`.
- The server keeps ciphertext in memory only, until the node takes the
  reply (or 15 minutes pass), and records only names and outcomes. Open
  forms' signed specs (no reply, sealed or not) are kept in `forms.json`
  next to the database, so a server restart keeps their links working until
  they expire; a reply held at the restart is dropped and must be sent again
  (`Forms::open` in `form.rs`).
  The node checks its signature, the expiry and one use, decrypts, and
  writes each value as a 0600 file in the target: the task's or bot's
  `$AXE_SECRETS_DIR` (`<run_root>/task-<id>/.secrets`, outside `repos/`;
  `env=VAR` fields are also set on its later runs), or a git credential for
  `--git <host>` that the node's clones fall back to
  (`[forms] git_credentials`).

The reverse, sealed messages to the owner (sealed messages in `sealed.rs`):

```sh
axe form reply-key --bot ops --wait   # once: the owner sets a passphrase
axe form send --bot ops --purpose "Staging DB password" --secret-file pw.txt
axe web login-link --tg --bot ops     # a one-time portal sign-in link, sealed
```

The owner's passphrase arrives through a sealed form; the node keeps
only the key derived from it (`[forms] reply_key_file`). `axe form send`
(`--secret-file`, `--link` or `--file`, 2 KiB in all) runs on the node
host, encrypts there and sends an inline "Open sealed message" Mini App
button whose form page link carries the ciphertext in its fragment. The page
asks for the passphrase and decrypts in the browser. Telegram and the server
see only the sender, purpose, expiry and ciphertext.
