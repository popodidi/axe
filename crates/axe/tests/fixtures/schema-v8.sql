-- Exact axe schema v8 (per-persona owner, sessions and runs).
CREATE TABLE IF NOT EXISTS dispatcher (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL,
 generation TEXT NOT NULL, expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS dispatch_ops (
 id TEXT PRIMARY KEY, dispatcher TEXT NOT NULL, request TEXT NOT NULL, response TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS dispatch_runs (
 run_id INTEGER PRIMARY KEY REFERENCES runs(id), dispatcher TEXT NOT NULL,
 generation TEXT NOT NULL, phase TEXT NOT NULL DEFAULT 'prepared'
);
CREATE TABLE IF NOT EXISTS dispatch_claims (
 claim TEXT PRIMARY KEY, dispatcher TEXT NOT NULL, generation TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY,
  project TEXT NOT NULL,
  title TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  owner TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending','in_progress','blocked','escalated','done')),
  claimed_by TEXT, runs INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  not_before TEXT
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL REFERENCES tasks(id),
  at TEXT NOT NULL, actor TEXT NOT NULL,
  session TEXT,
  kind TEXT NOT NULL,
  from_value TEXT, to_value TEXT, body TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
  task_id INTEGER NOT NULL REFERENCES tasks(id), persona TEXT NOT NULL,
  agent TEXT NOT NULL, session_id TEXT NOT NULL, last_event_id INTEGER NOT NULL,
  resume_count INTEGER NOT NULL, detail TEXT NOT NULL,
  PRIMARY KEY (task_id, persona)
);
CREATE TABLE IF NOT EXISTS rr (rule TEXT PRIMARY KEY, next INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL REFERENCES tasks(id),
  project TEXT NOT NULL, persona TEXT NOT NULL, agent TEXT NOT NULL, mode TEXT NOT NULL,
  session_id TEXT, claim TEXT NOT NULL,
  started_at TEXT NOT NULL, ended_at TEXT, exit_code INTEGER, log_path TEXT NOT NULL,
  current_activity TEXT, last_activity_at TEXT
);
CREATE INDEX IF NOT EXISTS runs_task ON runs(task_id);
CREATE INDEX IF NOT EXISTS runs_session ON runs(session_id);
CREATE INDEX IF NOT EXISTS tasks_owner_state ON tasks(owner, state);
CREATE INDEX IF NOT EXISTS events_task ON events(task_id);
CREATE TABLE IF NOT EXISTS deps (
  task_id INTEGER NOT NULL REFERENCES tasks(id),
  prereq_id INTEGER NOT NULL REFERENCES tasks(id),
  created_at TEXT NOT NULL,
  PRIMARY KEY (task_id, prereq_id),
  CHECK (task_id != prereq_id)
);
CREATE INDEX IF NOT EXISTS deps_prereq ON deps(prereq_id);
-- Retired dream lane: kept so v8 databases and fresh schemas stay identical.
CREATE TABLE IF NOT EXISTS dream_attempts (
  project TEXT NOT NULL,
  day TEXT NOT NULL,
  task_id INTEGER NOT NULL REFERENCES tasks(id),
  proposal TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK (revision > 0),
  disposition TEXT NOT NULL
    CHECK (disposition IN ('reserved','delivered','rejected','rework','abandoned')),
  reserved_at TEXT NOT NULL, actor TEXT NOT NULL,
  event_id INTEGER NOT NULL UNIQUE REFERENCES events(id),
  PRIMARY KEY (project, day)
);
CREATE INDEX IF NOT EXISTS dream_attempts_task ON dream_attempts(task_id);
