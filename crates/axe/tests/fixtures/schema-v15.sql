-- axe schema v15 (before goals), as a fresh v15 database creates it.
CREATE TABLE dispatcher (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL,
 generation TEXT NOT NULL, expires INTEGER NOT NULL
);
CREATE TABLE dispatch_ops (
 id TEXT PRIMARY KEY, dispatcher TEXT NOT NULL, request TEXT NOT NULL, response TEXT NOT NULL
);
CREATE TABLE dispatch_runs (
 run_id INTEGER PRIMARY KEY REFERENCES runs(id), dispatcher TEXT NOT NULL,
 generation TEXT NOT NULL, phase TEXT NOT NULL DEFAULT 'prepared'
);
CREATE TABLE dispatch_claims (
 claim TEXT PRIMARY KEY, dispatcher TEXT NOT NULL, generation TEXT NOT NULL
);
CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  project TEXT NOT NULL,
  title TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  state TEXT NOT NULL CHECK (state IN ('pending','in_progress','blocked','escalated','done')),
  claimed_by TEXT, runs INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  not_before TEXT,
  agent TEXT
);
CREATE TABLE events (
  id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL REFERENCES tasks(id),
  at TEXT NOT NULL, actor TEXT NOT NULL,
  session TEXT,
  kind TEXT NOT NULL,
  from_value TEXT, to_value TEXT, body TEXT
);
CREATE TABLE sessions (
  task_id INTEGER PRIMARY KEY REFERENCES tasks(id),
  agent TEXT NOT NULL, session_id TEXT NOT NULL, resume_count INTEGER NOT NULL
);
CREATE TABLE rr (rule TEXT PRIMARY KEY, next INTEGER NOT NULL);
CREATE TABLE runs (
  id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL REFERENCES tasks(id),
  project TEXT NOT NULL, agent TEXT NOT NULL, mode TEXT NOT NULL,
  session_id TEXT, claim TEXT NOT NULL,
  started_at TEXT NOT NULL, ended_at TEXT, exit_code INTEGER, log_path TEXT NOT NULL,
  current_activity TEXT, last_activity_at TEXT
);
CREATE INDEX runs_task ON runs(task_id);
CREATE INDEX runs_session ON runs(session_id);
CREATE INDEX events_task ON events(task_id);
CREATE TABLE deps (
  task_id INTEGER NOT NULL REFERENCES tasks(id),
  prereq_id INTEGER NOT NULL REFERENCES tasks(id),
  created_at TEXT NOT NULL,
  PRIMARY KEY (task_id, prereq_id),
  CHECK (task_id != prereq_id)
);
CREATE INDEX deps_prereq ON deps(prereq_id);
CREATE TABLE routines (
 id INTEGER PRIMARY KEY, config TEXT NOT NULL, enabled INTEGER NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0, next_at INTEGER NOT NULL,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE routine_occurrences (
 id INTEGER PRIMARY KEY, routine_id INTEGER NOT NULL REFERENCES routines(id),
 identity TEXT NOT NULL, scheduled_at INTEGER, task_id INTEGER NOT NULL UNIQUE REFERENCES tasks(id),
 snapshot TEXT NOT NULL, created_at TEXT NOT NULL,
 UNIQUE(routine_id, identity)
);
CREATE INDEX routine_history ON routine_occurrences(routine_id, id);
CREATE TABLE routine_events (
 id INTEGER PRIMARY KEY, routine_id INTEGER NOT NULL REFERENCES routines(id),
 at TEXT NOT NULL, actor TEXT NOT NULL, session TEXT, kind TEXT NOT NULL, body TEXT NOT NULL
);
CREATE TABLE bots (
 id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, project TEXT NOT NULL, agent TEXT NOT NULL,
 identity TEXT NOT NULL DEFAULT '', conduct TEXT NOT NULL DEFAULT '', notes TEXT NOT NULL DEFAULT '',
 heartbeat_seconds INTEGER NOT NULL DEFAULT 14400, nudge_after_seconds INTEGER NOT NULL DEFAULT 600,
 state TEXT NOT NULL CHECK (state IN ('active','paused')),
 session_agent TEXT, session_id TEXT, resume_count INTEGER NOT NULL DEFAULT 0,
 failures INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE bot_messages (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id),
 channel TEXT NOT NULL DEFAULT 'telegram', direction TEXT NOT NULL CHECK (direction IN ('in','out')),
 chat_id INTEGER NOT NULL, thread_id INTEGER, message_id INTEGER NOT NULL,
 reply_to INTEGER, edit_of INTEGER, from_id INTEGER, kind TEXT NOT NULL, text TEXT NOT NULL DEFAULT '',
 attachments TEXT NOT NULL DEFAULT '[]', media_group_id TEXT, flags TEXT NOT NULL DEFAULT '{}',
 at TEXT NOT NULL,
 UNIQUE (bot_id, channel, chat_id, message_id, kind)
);
CREATE TABLE bot_runs (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id), agent TEXT NOT NULL,
 mode TEXT NOT NULL, session_id TEXT, claim TEXT NOT NULL, dispatcher TEXT NOT NULL,
 generation TEXT NOT NULL, started_at TEXT NOT NULL, ended_at TEXT, exit_code INTEGER,
 stop_reason TEXT, stop_requested INTEGER NOT NULL DEFAULT 0, nudged_at TEXT,
 log_path TEXT NOT NULL DEFAULT '', current_activity TEXT, last_activity_at TEXT
);
CREATE TABLE bot_wakes (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id),
 kind TEXT NOT NULL CHECK (kind IN ('wake','message','button')), key TEXT NOT NULL,
 message_ref INTEGER REFERENCES bot_messages(id), body TEXT NOT NULL DEFAULT '',
 due_at TEXT NOT NULL, created_at TEXT NOT NULL, actor TEXT NOT NULL,
 run_id INTEGER REFERENCES bot_runs(id),
 delivered_via TEXT CHECK (delivered_via IN ('prompt','hook')),
 redelivered INTEGER NOT NULL DEFAULT 0,
 UNIQUE (bot_id, key)
);
CREATE TABLE bot_events (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id),
 at TEXT NOT NULL, actor TEXT NOT NULL, session TEXT, kind TEXT NOT NULL, body TEXT NOT NULL DEFAULT ''
);
CREATE TABLE bot_subscriptions (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id), project TEXT NOT NULL,
 wake_events TEXT NOT NULL, ride_events TEXT NOT NULL, batch_seconds INTEGER NOT NULL,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 UNIQUE (bot_id, project)
);
CREATE TABLE bot_feed (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id), event_id INTEGER NOT NULL,
 project TEXT NOT NULL, task_id INTEGER NOT NULL, event TEXT NOT NULL, title TEXT NOT NULL,
 mode TEXT NOT NULL CHECK (mode IN ('wake','ride')), wake_id INTEGER REFERENCES bot_wakes(id),
 run_id INTEGER REFERENCES bot_runs(id), at TEXT NOT NULL,
 UNIQUE (bot_id, event_id)
);
CREATE TABLE bot_schedules (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id), name TEXT NOT NULL,
 cron TEXT NOT NULL, timezone TEXT NOT NULL, objective TEXT NOT NULL,
 enabled INTEGER NOT NULL, next_at TEXT NOT NULL, last_fired_at TEXT,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 UNIQUE (bot_id, name)
);
CREATE INDEX bot_schedules_due ON bot_schedules(enabled, next_at);
CREATE INDEX bot_feed_wake ON bot_feed(wake_id);
CREATE INDEX bot_feed_ride ON bot_feed(bot_id, mode, run_id);
CREATE INDEX bot_feed_run ON bot_feed(run_id);
CREATE INDEX bot_runs_bot ON bot_runs(bot_id, id);
CREATE INDEX bot_wakes_pending ON bot_wakes(bot_id, run_id, due_at);
CREATE INDEX bot_events_bot ON bot_events(bot_id, id);
CREATE INDEX bot_messages_bot ON bot_messages(bot_id, id);
CREATE TABLE run_usage (
 kind TEXT NOT NULL CHECK (kind IN ('task','bot')), run_id INTEGER NOT NULL,
 status TEXT NOT NULL CHECK (status IN ('known','unknown','reset')), reason TEXT,
 session_id TEXT, turns INTEGER, context_tokens INTEGER,
 cache_start TEXT CHECK (cache_start IN ('cold','warm')),
 source TEXT NOT NULL, recorded_at TEXT NOT NULL,
 PRIMARY KEY (kind, run_id)
);
CREATE INDEX run_usage_session ON run_usage(session_id);
CREATE TABLE run_usage_models (
 kind TEXT NOT NULL, run_id INTEGER NOT NULL, model TEXT NOT NULL,
 total_input INTEGER NOT NULL, total_output INTEGER NOT NULL, total_thinking INTEGER NOT NULL,
 total_cache_read INTEGER NOT NULL, total_cache_write INTEGER NOT NULL, total_cost REAL NOT NULL,
 input INTEGER, output INTEGER, thinking INTEGER, cache_read INTEGER, cache_write INTEGER,
 cost REAL,
 PRIMARY KEY (kind, run_id, model),
 FOREIGN KEY (kind, run_id) REFERENCES run_usage(kind, run_id)
);
