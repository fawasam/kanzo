//! All reads and writes of `kanzo.db`. Plain SQL, one short-lived connection per operation.
//!
//! The database uses a rollback journal (`journal_mode = TRUNCATE`), not WAL. With a
//! connection per operation, WAL would set up and tear down its `-wal` and `-shm` files
//! on every request; the rollback journal makes a read touch no extra files at all and
//! measured about 3x faster per operation. Writes queue on the busy timeout.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// The oldest schema version whose Kanzo can still use a database at the latest one.
/// Adding a table or a column with a default leaves older versions working, so a board
/// synced from a newer Kanzo still opens in an older one. Raise it to the new version
/// only for a migration that older code can't live with, like a renamed column.
const COMPATIBLE_FROM: usize = 4;

/// Schema migrations, applied in order. Index + 1 is the `schema_version` they lead to.
const MIGRATIONS: &[&str] = &[
    include_str!("schema.sql"),
    // 2: a colour picked for the project; NULL leaves the board to choose one.
    "ALTER TABLE projects ADD COLUMN color TEXT;",
    // 3: a task's tags, comma-separated (see `clean_tags`).
    "ALTER TABLE tasks ADD COLUMN tags TEXT NOT NULL DEFAULT '';",
    // 4: any number of repos per project. Each repo belongs to one project. The old
    // projects.repo column stays, empty: SQLite can only drop it by rebuilding the table.
    "CREATE TABLE project_repos (
         repo       TEXT PRIMARY KEY,
         project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE
     );
     CREATE INDEX project_repos_by_project ON project_repos(project_id);
     INSERT INTO project_repos (repo, project_id) SELECT repo, id FROM projects WHERE repo IS NOT NULL;
     UPDATE projects SET repo = NULL;
     CREATE TRIGGER project_repos_insert AFTER INSERT ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER project_repos_delete AFTER DELETE ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 5: images attached to a task, or to one of its comments; the files are in images/.
    "CREATE TABLE images (
         id         INTEGER PRIMARY KEY AUTOINCREMENT,
         task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         comment_id INTEGER REFERENCES comments(id) ON DELETE CASCADE,
         file       TEXT NOT NULL,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE INDEX images_by_task ON images(task_id);
     CREATE TRIGGER images_insert AFTER INSERT ON images BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER images_delete AFTER DELETE ON images BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 6: tasks for a person, which agents leave alone.
    "ALTER TABLE tasks ADD COLUMN human INTEGER NOT NULL DEFAULT 0;",
    // 7: an optional GitLab.com merge request linked to a task. Tokens deliberately
    // never enter this synced database; see gitlab.rs.
    "ALTER TABLE tasks ADD COLUMN gitlab_mr TEXT;",
    // 8: the personal planning foundation. All links are optional so existing boards
    // stay valid, while a project belongs to at most one life area.
    "CREATE TABLE life_areas (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         slug TEXT NOT NULL UNIQUE,
         name TEXT NOT NULL,
         color TEXT,
         position REAL NOT NULL,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE TABLE journeys (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         life_area_id INTEGER NOT NULL REFERENCES life_areas(id) ON DELETE CASCADE,
         title TEXT NOT NULL,
         purpose TEXT NOT NULL DEFAULT '',
         target_date TEXT,
         status TEXT NOT NULL DEFAULT 'active',
         position REAL NOT NULL,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE TABLE milestones (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         journey_id INTEGER NOT NULL REFERENCES journeys(id) ON DELETE CASCADE,
         title TEXT NOT NULL,
         target_date TEXT,
         status TEXT NOT NULL DEFAULT 'planned',
         position REAL NOT NULL,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE INDEX journeys_by_area ON journeys(life_area_id, position);
     CREATE INDEX milestones_by_journey ON milestones(journey_id, position);
     ALTER TABLE projects ADD COLUMN life_area_id INTEGER REFERENCES life_areas(id) ON DELETE SET NULL;
     ALTER TABLE tasks ADD COLUMN journey_id INTEGER REFERENCES journeys(id) ON DELETE SET NULL;
     ALTER TABLE tasks ADD COLUMN milestone_id INTEGER REFERENCES milestones(id) ON DELETE SET NULL;
     CREATE TRIGGER life_areas_insert AFTER INSERT ON life_areas BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER life_areas_update AFTER UPDATE ON life_areas BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER life_areas_delete AFTER DELETE ON life_areas BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER journeys_insert AFTER INSERT ON journeys BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER journeys_update AFTER UPDATE ON journeys BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER journeys_delete AFTER DELETE ON journeys BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER milestones_insert AFTER INSERT ON milestones BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER milestones_update AFTER UPDATE ON milestones BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER milestones_delete AFTER DELETE ON milestones BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 9: the daily execution layer. Dates are calendar dates in the user's local
    // browser, and priorities are deliberately limited by the API to three per day.
    "ALTER TABLE tasks ADD COLUMN scheduled_for TEXT;
     CREATE INDEX tasks_by_scheduled_for ON tasks(scheduled_for, position);
     CREATE TABLE daily_priorities (
         day TEXT NOT NULL,
         task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         position INTEGER NOT NULL,
         PRIMARY KEY(day, task_id)
     );
     CREATE INDEX daily_priorities_by_day ON daily_priorities(day, position);
     CREATE TRIGGER daily_priorities_insert AFTER INSERT ON daily_priorities BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER daily_priorities_delete AFTER DELETE ON daily_priorities BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 10: a short list of outcomes gives the week a direction beyond its scheduled tasks.
    "CREATE TABLE weekly_outcomes (
         week_start TEXT NOT NULL,
         position INTEGER NOT NULL,
         title TEXT NOT NULL,
         PRIMARY KEY(week_start, position)
     );
     CREATE TRIGGER weekly_outcomes_insert AFTER INSERT ON weekly_outcomes BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER weekly_outcomes_update AFTER UPDATE ON weekly_outcomes BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER weekly_outcomes_delete AFTER DELETE ON weekly_outcomes BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 11: lightweight recurring habits and their local-calendar check-ins.
    "CREATE TABLE habits (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         title TEXT NOT NULL,
         position REAL NOT NULL,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE TABLE habit_checks (
         habit_id INTEGER NOT NULL REFERENCES habits(id) ON DELETE CASCADE,
         day TEXT NOT NULL,
         PRIMARY KEY(habit_id, day)
     );
     CREATE INDEX habit_checks_by_day ON habit_checks(day);
     CREATE TRIGGER habits_insert AFTER INSERT ON habits BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER habits_update AFTER UPDATE ON habits BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER habits_delete AFTER DELETE ON habits BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER habit_checks_insert AFTER INSERT ON habit_checks BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER habit_checks_delete AFTER DELETE ON habit_checks BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    // 12: local job records and their in-app notifications. The provider boundary is
    // deliberately import-only: no scraping, credentials, emails or schedulers run.
    "CREATE TABLE job_preferences (
         id INTEGER PRIMARY KEY CHECK(id = 1), skills TEXT NOT NULL, locations TEXT NOT NULL,
         excluded_requirements TEXT NOT NULL, preferred_age_hours INTEGER NOT NULL DEFAULT 24,
         fallback_age_days INTEGER NOT NULL DEFAULT 7, notifications_enabled INTEGER NOT NULL DEFAULT 1
     );
     INSERT INTO job_preferences(id,skills,locations,excluded_requirements) VALUES(1,'react,next.js,node.js,typescript,python,mern','kerala,bengaluru,dubai,gcc','java,.net');
     CREATE TABLE jobs (
         id INTEGER PRIMARY KEY AUTOINCREMENT, company TEXT NOT NULL, title TEXT NOT NULL,
         location TEXT NOT NULL DEFAULT '', url TEXT, source TEXT NOT NULL DEFAULT '', posted_at TEXT,
         source_date_note TEXT NOT NULL DEFAULT '', description TEXT NOT NULL DEFAULT '',
         stage TEXT NOT NULL DEFAULT 'saved', notes TEXT NOT NULL DEFAULT '', follow_up_on TEXT,
         dedupe_key TEXT NOT NULL UNIQUE, match_score INTEGER NOT NULL DEFAULT 0,
         match_reasons TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
         updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE INDEX jobs_search ON jobs(stage, posted_at, match_score);
     CREATE TABLE job_notifications (
         id INTEGER PRIMARY KEY AUTOINCREMENT, job_id INTEGER NOT NULL UNIQUE REFERENCES jobs(id) ON DELETE CASCADE,
         message TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')), read_at TEXT
     );
     CREATE TRIGGER jobs_insert AFTER INSERT ON jobs BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER jobs_update AFTER UPDATE ON jobs BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER job_notifications_insert AFTER INSERT ON job_notifications BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER job_notifications_update AFTER UPDATE ON job_notifications BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
];

/// The colours a project can be given; the board maps each name to a shade.
const PROJECT_COLORS: &[&str] = &["gray", "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
    /// Another process holds the data folder for longer than we wait, like a long `kanzo sync`.
    Busy(String),
    Db(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(m) | Error::Invalid(m) | Error::Busy(m) => f.write_str(m),
            Error::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub name: String,
    /// Linked GitHub repos as `owner/repo`, in the order they were linked.
    pub repos: Vec<String>,
    /// One of `PROJECT_COLORS`, or `None` when never picked.
    pub color: Option<String>,
    pub life_area_id: Option<i64>,
    pub created_at: String,
    /// Tasks not in the last (done) column.
    pub open: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub description: String,
    /// Lowercase, without duplicates, in the order they were given.
    pub tags: Vec<String>,
    /// Work for a person: agents leave it alone, and `next_task` never picks it.
    pub human: bool,
    /// A canonical GitLab.com merge request URL, when one is linked.
    pub gitlab_mr: Option<String>,
    pub journey_id: Option<i64>,
    pub milestone_id: Option<i64>,
    /// A local calendar date, `YYYY-MM-DD`, when the user intends to do it.
    pub scheduled_for: Option<String>,
    pub status: String,
    pub position: f64,
    pub created_at: String,
    pub updated_at: String,
    pub comments: i64,
}

impl Task {
    /// How tasks are shown everywhere: `api#12`.
    pub fn key(&self) -> String {
        format!("{}#{}", self.project, self.id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: i64,
    pub task_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// An image attached to a task, or to one of its comments.
#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub id: i64,
    pub task_id: i64,
    pub comment_id: Option<i64>,
    /// Its file name in the data folder's `images/`, like `6fec521a01297ab5.png`.
    pub file: String,
}

/// Fields of a task to change; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    /// Moves the task to this project (by id).
    pub project_id: Option<i64>,
    /// Comma-separated; replaces all the tags, and an empty string clears them.
    pub tags: Option<String>,
    pub human: Option<bool>,
    /// A GitLab.com merge request URL; an empty value clears the link.
    pub gitlab_mr: Option<String>,
    /// `Some(None)` clears the link.
    pub journey_id: Option<Option<i64>>,
    /// `Some(None)` clears the link.
    pub milestone_id: Option<Option<i64>>,
    /// `Some(None)` removes the scheduled day.
    pub scheduled_for: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LifeArea {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Journey {
    pub id: i64,
    pub life_area_id: i64,
    pub title: String,
    pub purpose: String,
    pub target_date: Option<String>,
    pub status: String,
    pub milestones_done: i64,
    pub milestones_total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Milestone {
    pub id: i64,
    pub journey_id: i64,
    pub title: String,
    pub target_date: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Habit {
    pub id: i64,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HabitCheck {
    pub habit_id: i64,
    pub day: String,
}

fn journey_row(r: &Row) -> rusqlite::Result<Journey> {
    Ok(Journey {
        id: r.get(0)?,
        life_area_id: r.get(1)?,
        title: r.get(2)?,
        purpose: r.get(3)?,
        target_date: r.get(4)?,
        status: r.get(5)?,
        milestones_done: r.get(6)?,
        milestones_total: r.get(7)?,
    })
}

fn milestone_row(r: &Row) -> rusqlite::Result<Milestone> {
    Ok(Milestone { id: r.get(0)?, journey_id: r.get(1)?, title: r.get(2)?, target_date: r.get(3)?, status: r.get(4)? })
}

/// The board's columns, left to right. New tasks go in todo, agents take theirs from
/// there and move them to doing, and tasks in done count as done.
const STATUSES: [&str; 4] = ["todo", "doing", "review", "done"];

/// An optional column before todo, for tasks that agents leave alone.
pub const BACKLOG: &str = "backlog";

pub struct Store {
    pub(crate) conn: Connection,
    statuses: Vec<String>,
    /// Held while the connection is open; declared after `conn` so it is released after it closes.
    _lock: Option<std::fs::File>,
}

impl Store {
    /// Opens the database. `lock` is kept until the store is dropped (see `Home::store`).
    pub fn open(path: &Path, lock: Option<std::fs::File>) -> Result<Self> {
        let conn = Connection::open(path)?;
        // The board and any number of agents share this file; wait instead of failing.
        conn.busy_timeout(Duration::from_secs(5))?;
        // Per connection, not stored in the file. Also turns a WAL database from an
        // older kanzo back into a rollback-journal one once nothing else has it open.
        // `synchronous` stays at SQLite's default FULL, which keeps the file intact
        // when a board loses power.
        conn.pragma_update_and_check(None, "journal_mode", "TRUNCATE", |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Before the first migration there is no meta table, and so no backlog.
        let backlog = conn
            .query_row("SELECT value FROM meta WHERE key = 'backlog'", [], |r| r.get::<_, String>(0))
            .is_ok_and(|v| v == "1");
        let statuses = backlog.then_some(BACKLOG).into_iter().chain(STATUSES).map(String::from).collect();
        Ok(Self { conn, statuses, _lock: lock })
    }

    pub fn statuses(&self) -> &[String] {
        &self.statuses
    }

    fn done_status(&self) -> &str {
        STATUSES[3]
    }

    /// Shows or hides the backlog column. It must be empty to hide it, or its
    /// tasks would vanish from the board.
    pub fn set_backlog(&self, on: bool) -> Result<()> {
        self.write(|c| {
            let left: i64 = c.query_row("SELECT count(*) FROM tasks WHERE status = ?1", [BACKLOG], |r| r.get(0))?;
            if !on && left > 0 {
                let tasks = if left == 1 { "task" } else { "tasks" };
                return Err(Error::Invalid(format!("move the {left} {tasks} out of backlog first")));
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('backlog', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [if on { "1" } else { "0" }],
            )?;
            // Other open boards redraw their columns.
            c.execute("UPDATE meta SET value = value + 1 WHERE key = 'changes'", [])?;
            Ok(())
        })
    }

    /// Runs `f` in an immediate transaction, so concurrent writers queue up
    /// on the busy timeout instead of failing halfway.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&self.conn) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Brings the schema up to date. Writes nothing when it already is, so starting
    /// `kanzo mcp` for every agent session costs no disk write.
    pub fn migrate(&self) -> Result<()> {
        let meta = |c: &Connection, key: &str| -> rusqlite::Result<Option<usize>> {
            Ok(c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get::<_, String>(0))
                .optional()?
                .and_then(|v| v.parse().ok()))
        };
        let latest = MIGRATIONS.len();
        let current =
            (meta(&self.conn, "schema_version").unwrap_or(None), meta(&self.conn, "compatible").unwrap_or(None));
        if current == (Some(latest), Some(COMPATIBLE_FROM)) {
            return Ok(());
        }
        self.write(|c| {
            c.execute("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", [])?;
            let version = meta(c, "schema_version")?.unwrap_or(0);
            if version > latest {
                // From a newer Kanzo, maybe synced from another machine: use it as it is
                // when this version can, without touching its schema.
                if meta(c, "compatible")?.is_some_and(|oldest| oldest <= latest) {
                    return Ok(());
                }
                return Err(Error::Invalid(
                    "this board was saved by a newer version of Kanzo; update this one with `kanzo update`".into(),
                ));
            }
            for sql in &MIGRATIONS[version..] {
                c.execute_batch(sql)?;
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('compatible', ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [latest.to_string(), COMPATIBLE_FROM.to_string()],
            )?;
            Ok(())
        })
    }

    /// Bumped by triggers on every write; the board polls it.
    pub fn changes(&self) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'changes'")?
            .query_row([], |r| r.get(0))?)
    }

    /// The current time, in the same format as the stored timestamps.
    pub fn now(&self) -> Result<String> {
        Ok(self.conn.query_row(&format!("SELECT {NOW}"), [], |r| r.get(0))?)
    }

    /// Makes `kanzo.db` complete on its own: rolls back a write that a crashed process
    /// left half done, and folds in a WAL left by an older kanzo.
    pub fn settle(&self) -> Result<()> {
        self.conn.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))?;
        Ok(())
    }

    // Projects

    const PROJECT_SELECT: &str = "SELECT p.id, p.slug, p.name,
            (SELECT group_concat(repo, ',' ORDER BY r.rowid) FROM project_repos r WHERE r.project_id = p.id),
            p.color, p.life_area_id, p.created_at,
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id AND t.status <> ?1),
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id)
        FROM projects p";

    fn project_row(r: &Row) -> rusqlite::Result<Project> {
        Ok(Project {
            id: r.get(0)?,
            slug: r.get(1)?,
            name: r.get(2)?,
            repos: r.get::<_, Option<String>>(3)?.map(|s| s.split(',').map(String::from).collect()).unwrap_or_default(),
            color: r.get(4)?,
            life_area_id: r.get(5)?,
            created_at: r.get(6)?,
            open: r.get(7)?,
            total: r.get(8)?,
        })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let sql = format!("{} ORDER BY p.slug", Self::PROJECT_SELECT);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([self.done_status()], Self::project_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn project_where(&self, clause: &str, value: &str) -> Result<Option<Project>> {
        let sql = format!("{} WHERE {clause}", Self::PROJECT_SELECT);
        Ok(self.conn.query_row(&sql, params![self.done_status(), value], Self::project_row).optional()?)
    }

    pub fn project_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        self.project_where("p.slug = ?2", &slug.trim().to_ascii_lowercase())
    }

    pub fn project_by_repo(&self, repo: &str) -> Result<Option<Project>> {
        match repo_from_url(repo) {
            Some(repo) => self.project_where("p.id = (SELECT project_id FROM project_repos WHERE repo = ?2)", &repo),
            None => Ok(None),
        }
    }

    /// Finds a project by slug, or by linked repo when given `owner/repo`.
    pub fn find_project(&self, name: &str) -> Result<Project> {
        let found = match self.project_by_slug(name)? {
            Some(p) => Some(p),
            None if name.contains('/') => self.project_by_repo(name)?,
            None => None,
        };
        found.ok_or_else(|| {
            let known = self.list_projects().unwrap_or_default();
            let names: Vec<_> = known.iter().map(|p| p.slug.as_str()).collect();
            let hint = if names.is_empty() {
                "there are no projects yet".to_string()
            } else {
                format!("projects: {}", names.join(", "))
            };
            Error::NotFound(format!("no project {name:?} ({hint})"))
        })
    }

    pub fn create_project(&self, slug: &str, name: &str, repo: Option<&str>) -> Result<Project> {
        let slug = slug.trim().to_ascii_lowercase();
        if !valid_slug(&slug) {
            return Err(Error::Invalid(format!(
                "project slug {slug:?} must be 1-32 lowercase letters, digits or -, starting with a letter or digit"
            )));
        }
        let name = match name.trim() {
            "" => slug.clone(),
            n => n.to_string(),
        };
        let repo = repo.map(str::trim).filter(|r| !r.is_empty()).map(clean_repo).transpose()?;
        if self.project_by_slug(&slug)?.is_some() {
            return Err(Error::Invalid(format!("project {slug} already exists")));
        }
        if let Some(r) = &repo {
            self.check_repo_free(r)?;
        }
        let created = self.write(|c| {
            c.execute("INSERT INTO projects (slug, name) VALUES (?1, ?2)", params![slug, name])?;
            if let Some(r) = &repo {
                c.execute(
                    "INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)",
                    params![r, c.last_insert_rowid()],
                )?;
            }
            Ok(())
        });
        match created {
            // Someone created the same project or linked the repo between our checks and the insert.
            Err(Error::Db(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(Error::Invalid(format!("project {slug} or its repo already exists")))
            }
            other => other,
        }?;
        self.find_project(&slug)
    }

    /// Links another repo to a project. Each repo belongs to one project.
    pub fn link_repo(&self, slug: &str, repo: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        let repo = clean_repo(repo)?;
        if project.repos.contains(&repo) {
            return Ok(project);
        }
        self.check_repo_free(&repo)?;
        match self
            .conn
            .execute("INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)", params![repo, project.id])
        {
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                return Err(Error::Invalid(format!("repo {repo} is already linked to another project")));
            }
            other => other?,
        };
        self.find_project(&project.slug)
    }

    pub fn unlink_repo(&self, slug: &str, repo: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        let repo = repo_from_url(repo).unwrap_or_else(|| repo.trim().to_ascii_lowercase());
        let removed = self
            .conn
            .execute("DELETE FROM project_repos WHERE repo = ?1 AND project_id = ?2", params![repo, project.id])?;
        if removed == 0 {
            return Err(Error::NotFound(format!("repo {repo} is not linked to project {}", project.slug)));
        }
        self.find_project(&project.slug)
    }

    fn check_repo_free(&self, repo: &str) -> Result<()> {
        match self.project_by_repo(repo)? {
            Some(other) => Err(Error::Invalid(format!("repo {repo} is already linked to project {}", other.slug))),
            None => Ok(()),
        }
    }

    /// Renames a project. Its slug, and so its task keys like api#12, stay.
    pub fn rename_project(&self, slug: &str, name: &str) -> Result<Project> {
        let name = squash(name);
        if name.is_empty() {
            return Err(Error::Invalid("project name is empty".into()));
        }
        let project = self.find_project(slug)?;
        self.conn.execute("UPDATE projects SET name = ?1 WHERE id = ?2", params![name, project.id])?;
        self.find_project(&project.slug)
    }

    pub fn set_project_color(&self, slug: &str, color: &str) -> Result<Project> {
        if !PROJECT_COLORS.contains(&color) {
            return Err(Error::Invalid(format!("unknown color {color:?}; use one of: {}", PROJECT_COLORS.join(", "))));
        }
        let project = self.find_project(slug)?;
        self.conn.execute("UPDATE projects SET color = ?1 WHERE id = ?2", params![color, project.id])?;
        self.find_project(&project.slug)
    }

    /// Associates a project with one optional life area. `None` leaves it ungrouped.
    pub fn set_project_life_area(&self, slug: &str, life_area_id: Option<i64>) -> Result<Project> {
        if let Some(id) = life_area_id {
            self.get_life_area(id)?;
        }
        let project = self.find_project(slug)?;
        self.conn.execute("UPDATE projects SET life_area_id = ?1 WHERE id = ?2", params![life_area_id, project.id])?;
        self.find_project(&project.slug)
    }

    /// Deletes a project with all its tasks and their comments; returns the project as it was.
    pub fn delete_project(&self, slug: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        // Tasks and comments go with it through ON DELETE CASCADE.
        self.conn.execute("DELETE FROM projects WHERE id = ?1", [project.id])?;
        Ok(project)
    }

    // Personal planning

    pub fn list_life_areas(&self) -> Result<Vec<LifeArea>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT id, slug, name, color FROM life_areas ORDER BY position, id")?;
        Ok(stmt
            .query_map([], |r| Ok(LifeArea { id: r.get(0)?, slug: r.get(1)?, name: r.get(2)?, color: r.get(3)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_life_area(&self, id: i64) -> Result<LifeArea> {
        self.conn
            .query_row("SELECT id, slug, name, color FROM life_areas WHERE id = ?1", [id], |r| {
                Ok(LifeArea { id: r.get(0)?, slug: r.get(1)?, name: r.get(2)?, color: r.get(3)? })
            })
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no life area #{id}")))
    }

    pub fn create_life_area(&self, slug: &str, name: &str, color: Option<&str>) -> Result<LifeArea> {
        let slug = slug.trim().to_ascii_lowercase();
        if !valid_slug(&slug) {
            return Err(Error::Invalid("life area slug must use lowercase letters, digits or -".into()));
        }
        let name = if name.trim().is_empty() { slug.clone() } else { squash(name) };
        let color = color.filter(|c| !c.is_empty());
        if let Some(color) = color
            && !PROJECT_COLORS.contains(&color)
        {
            return Err(Error::Invalid(format!("unknown color {color:?}")));
        }
        let position: f64 =
            self.conn.query_row("SELECT coalesce(max(position), 0) + 1 FROM life_areas", [], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO life_areas (slug, name, color, position) VALUES (?1, ?2, ?3, ?4)",
            params![slug, name, color, position],
        )?;
        self.get_life_area(self.conn.last_insert_rowid())
    }

    pub fn list_journeys(&self) -> Result<Vec<Journey>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT j.id, j.life_area_id, j.title, j.purpose, j.target_date, j.status,
                (SELECT count(*) FROM milestones m WHERE m.journey_id = j.id AND m.status = 'complete'),
                (SELECT count(*) FROM milestones m WHERE m.journey_id = j.id)
             FROM journeys j ORDER BY j.position, j.id",
        )?;
        Ok(stmt.query_map([], journey_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_journey(&self, id: i64) -> Result<Journey> {
        self.conn
            .query_row(
                "SELECT j.id, j.life_area_id, j.title, j.purpose, j.target_date, j.status,
                    (SELECT count(*) FROM milestones m WHERE m.journey_id = j.id AND m.status = 'complete'),
                    (SELECT count(*) FROM milestones m WHERE m.journey_id = j.id)
                 FROM journeys j WHERE j.id = ?1",
                [id],
                journey_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no journey #{id}")))
    }

    pub fn create_journey(
        &self,
        life_area_id: i64,
        title: &str,
        purpose: &str,
        target_date: Option<&str>,
    ) -> Result<Journey> {
        self.get_life_area(life_area_id)?;
        let title = clean_title(title)?;
        let position: f64 = self.conn.query_row(
            "SELECT coalesce(max(position), 0) + 1 FROM journeys WHERE life_area_id = ?1",
            [life_area_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO journeys (life_area_id, title, purpose, target_date, position) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                life_area_id,
                title,
                purpose.trim(),
                target_date.map(str::trim).filter(|d| !d.is_empty()),
                position
            ],
        )?;
        self.get_journey(self.conn.last_insert_rowid())
    }

    pub fn list_milestones(&self) -> Result<Vec<Milestone>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, journey_id, title, target_date, status FROM milestones ORDER BY position, id",
        )?;
        Ok(stmt.query_map([], milestone_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_milestone(&self, id: i64) -> Result<Milestone> {
        self.conn
            .query_row(
                "SELECT id, journey_id, title, target_date, status FROM milestones WHERE id = ?1",
                [id],
                milestone_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no milestone #{id}")))
    }

    pub fn create_milestone(&self, journey_id: i64, title: &str, target_date: Option<&str>) -> Result<Milestone> {
        self.get_journey(journey_id)?;
        let title = clean_title(title)?;
        let position: f64 = self.conn.query_row(
            "SELECT coalesce(max(position), 0) + 1 FROM milestones WHERE journey_id = ?1",
            [journey_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO milestones (journey_id, title, target_date, position) VALUES (?1, ?2, ?3, ?4)",
            params![journey_id, title, target_date.map(str::trim).filter(|d| !d.is_empty()), position],
        )?;
        self.get_milestone(self.conn.last_insert_rowid())
    }

    // Tasks

    const TASK_SELECT: &str = "SELECT t.id, p.slug, t.title, t.description, t.status, t.position,
            t.created_at, t.updated_at, (SELECT count(*) FROM comments c WHERE c.task_id = t.id), t.tags, t.human, t.gitlab_mr, t.journey_id, t.milestone_id, t.scheduled_for
        FROM tasks t JOIN projects p ON p.id = t.project_id";

    fn task_row(r: &Row) -> rusqlite::Result<Task> {
        Ok(Task {
            id: r.get(0)?,
            project: r.get(1)?,
            title: r.get(2)?,
            description: r.get(3)?,
            tags: r.get::<_, String>(9)?.split(',').filter(|t| !t.is_empty()).map(String::from).collect(),
            human: r.get(10)?,
            gitlab_mr: r.get(11)?,
            journey_id: r.get(12)?,
            milestone_id: r.get(13)?,
            scheduled_for: r.get(14)?,
            status: r.get(4)?,
            position: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            comments: r.get(8)?,
        })
    }

    /// Tasks in board order: by column as configured, then by position.
    /// Tasks whose status is no longer in the config sort last.
    pub fn list_tasks(&self, project_id: Option<i64>, status: Option<&str>) -> Result<Vec<Task>> {
        Ok(self.list_tasks_keeping_done(project_id, status, None)?.0)
    }

    /// `list_tasks`, but of the done column, which only grows, just the newest `keep_done`
    /// tasks, read from the database without the rest; also says how many were left out.
    pub fn list_tasks_keeping_done(
        &self,
        project_id: Option<i64>,
        status: Option<&str>,
        keep_done: Option<usize>,
    ) -> Result<(Vec<Task>, usize)> {
        let done = self.done_status();
        // SQLite reads a negative LIMIT as no limit.
        let limit = keep_done.map_or(-1, |n| n as i64);
        let sql = format!(
            "{} WHERE (?1 IS NULL OR t.project_id = ?1) AND (?2 IS NULL OR t.status = ?2)
               AND (t.status <> ?3 OR t.id IN (SELECT id FROM tasks WHERE status = ?3
                    AND (?1 IS NULL OR project_id = ?1) ORDER BY position DESC LIMIT ?4))
             ORDER BY t.position",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut tasks: Vec<Task> = stmt
            .query_map(params![project_id, status, done, limit], Self::task_row)?
            .collect::<rusqlite::Result<_>>()?;
        let column = |t: &Task| self.statuses().iter().position(|s| *s == t.status).unwrap_or(usize::MAX);
        tasks.sort_by_key(column);
        let hidden = match keep_done {
            Some(keep) if status.is_none_or(|s| s == done) => {
                let all: i64 = self.conn.query_row(
                    "SELECT count(*) FROM tasks WHERE status = ?1 AND (?2 IS NULL OR project_id = ?2)",
                    params![done, project_id],
                    |r| r.get(0),
                )?;
                usize::try_from(all).unwrap_or(0).saturating_sub(keep)
            }
            _ => 0,
        };
        Ok((tasks, hidden))
    }

    /// Tasks of any project whose key, title, description, tags or project hold every
    /// word of `query`: open ones first, then the most recently changed. An empty query
    /// gives the most recently changed tasks.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Task>> {
        let words: Vec<String> = query.split_whitespace().take(10).map(|w| w.to_lowercase()).collect();
        let mut sql = format!("{} WHERE 1", Self::TASK_SELECT);
        for i in 0..words.len() {
            sql.push_str(&format!(
                " AND lower(p.slug || '#' || t.id || ' ' || t.title || ' ' || t.description || ' ' || t.tags) \
                 LIKE ?{} ESCAPE '\\'",
                i + 2
            ));
        }
        sql.push_str(&format!(" ORDER BY t.status = ?1, t.updated_at DESC, t.id DESC LIMIT {limit}"));
        let mut values: Vec<String> = vec![self.done_status().to_string()];
        for w in &words {
            let escaped = w.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            values.push(format!("%{escaped}%"));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), Self::task_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_task(&self, id: i64) -> Result<Task> {
        let sql = format!("{} WHERE t.id = ?1", Self::TASK_SELECT);
        self.conn
            .prepare_cached(&sql)?
            .query_row([id], Self::task_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no task #{id}")))
    }

    /// Tasks deliberately scheduled for a local calendar day, in their board order.
    pub fn list_scheduled_tasks(&self, day: &str) -> Result<Vec<Task>> {
        let day = clean_day(day)?;
        let sql = format!("{} WHERE t.scheduled_for = ?1 ORDER BY t.position", Self::TASK_SELECT);
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_map([day], Self::task_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// The small, intentional list of tasks that define a particular day's success.
    pub fn list_daily_priorities(&self, day: &str) -> Result<Vec<Task>> {
        let day = clean_day(day)?;
        let sql = format!(
            "{} JOIN daily_priorities d ON d.task_id = t.id WHERE d.day = ?1 ORDER BY d.position",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_map([day], Self::task_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_weekly_outcomes(&self, week_start: &str) -> Result<Vec<String>> {
        let week_start = clean_day(week_start)?;
        let mut stmt =
            self.conn.prepare("SELECT title FROM weekly_outcomes WHERE week_start = ?1 ORDER BY position")?;
        Ok(stmt.query_map([week_start], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Saves between zero and five concrete outcomes for a calendar week.
    pub fn set_weekly_outcomes(&self, week_start: &str, titles: &[String]) -> Result<Vec<String>> {
        let week_start = clean_day(week_start)?;
        if titles.len() > 5 {
            return Err(Error::Invalid("choose at most five weekly outcomes".into()));
        }
        let mut cleaned = Vec::new();
        for title in titles {
            let title = clean_title(title)?;
            if !cleaned.contains(&title) {
                cleaned.push(title);
            }
        }
        self.write(|c| {
            c.execute("DELETE FROM weekly_outcomes WHERE week_start = ?1", [&week_start])?;
            for (position, title) in cleaned.iter().enumerate() {
                c.execute(
                    "INSERT INTO weekly_outcomes (week_start, position, title) VALUES (?1, ?2, ?3)",
                    params![week_start, position as i64 + 1, title],
                )?;
            }
            Ok(())
        })?;
        Ok(cleaned)
    }

    /// Every scheduled task in an inclusive local-date range.
    pub fn list_tasks_scheduled_between(&self, start: &str, end: &str) -> Result<Vec<Task>> {
        let (start, end) = (clean_day(start)?, clean_day(end)?);
        if start > end {
            return Err(Error::Invalid("week start must be before its end".into()));
        }
        let sql = format!(
            "{} WHERE t.scheduled_for >= ?1 AND t.scheduled_for <= ?2 ORDER BY t.scheduled_for, t.position",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_map(params![start, end], Self::task_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Open scheduled work from before the selected week that needs a conscious decision.
    pub fn list_carry_forward_tasks(&self, before: &str) -> Result<Vec<Task>> {
        let before = clean_day(before)?;
        let sql = format!(
            "{} WHERE t.scheduled_for < ?1 AND t.status <> ?2 ORDER BY t.scheduled_for, t.updated_at",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_map(params![before, self.done_status()], Self::task_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_habits(&self) -> Result<Vec<Habit>> {
        let mut stmt = self.conn.prepare("SELECT id, title, created_at FROM habits ORDER BY position, id")?;
        Ok(stmt
            .query_map([], |row| Ok(Habit { id: row.get(0)?, title: row.get(1)?, created_at: row.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn create_habit(&self, title: &str) -> Result<Habit> {
        let title = clean_title(title)?;
        let id = self.write(|c| {
            let position: f64 =
                c.query_row("SELECT coalesce(max(position), 0) + 1 FROM habits", [], |row| row.get(0))?;
            c.execute("INSERT INTO habits (title, position) VALUES (?1, ?2)", params![title, position])?;
            Ok(c.last_insert_rowid())
        })?;
        self.conn
            .query_row("SELECT id, title, created_at FROM habits WHERE id = ?1", [id], |row| {
                Ok(Habit { id: row.get(0)?, title: row.get(1)?, created_at: row.get(2)? })
            })
            .map_err(Into::into)
    }

    pub fn list_habit_checks(&self, start: &str, end: &str) -> Result<Vec<HabitCheck>> {
        let (start, end) = (clean_day(start)?, clean_day(end)?);
        if start > end {
            return Err(Error::Invalid("habit range start must be before its end".into()));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT habit_id, day FROM habit_checks WHERE day >= ?1 AND day <= ?2 ORDER BY habit_id, day")?;
        Ok(stmt
            .query_map(params![start, end], |row| Ok(HabitCheck { habit_id: row.get(0)?, day: row.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_habit_check(&self, habit_id: i64, day: &str, done: bool) -> Result<()> {
        let day = clean_day(day)?;
        self.write(|c| {
            let exists: Option<i64> =
                c.query_row("SELECT id FROM habits WHERE id = ?1", [habit_id], |row| row.get(0)).optional()?;
            if exists.is_none() {
                return Err(Error::NotFound(format!("no habit #{habit_id}")));
            }
            if done {
                c.execute(
                    "INSERT OR IGNORE INTO habit_checks (habit_id, day) VALUES (?1, ?2)",
                    params![habit_id, day],
                )?;
            } else {
                c.execute("DELETE FROM habit_checks WHERE habit_id = ?1 AND day = ?2", params![habit_id, day])?;
            }
            Ok(())
        })
    }

    /// Replaces a day's priorities, preserving the caller's order. Three keeps the
    /// list useful as a focus tool rather than another long task list.
    pub fn set_daily_priorities(&self, day: &str, task_ids: &[i64]) -> Result<Vec<Task>> {
        let day = clean_day(day)?;
        if task_ids.len() > 3 {
            return Err(Error::Invalid("choose at most three daily priorities".into()));
        }
        if task_ids.iter().enumerate().any(|(i, id)| task_ids[..i].contains(id)) {
            return Err(Error::Invalid("a daily priority can only appear once".into()));
        }
        for id in task_ids {
            self.get_task(*id)?;
        }
        self.write(|c| {
            c.execute("DELETE FROM daily_priorities WHERE day = ?1", [&day])?;
            for (position, task_id) in task_ids.iter().enumerate() {
                c.execute(
                    "INSERT INTO daily_priorities (day, task_id, position) VALUES (?1, ?2, ?3)",
                    params![day, task_id, position as i64 + 1],
                )?;
            }
            Ok(())
        })?;
        self.list_daily_priorities(&day)
    }

    /// Looks up `api#12`, `#12` or `12`. A project prefix must match the task's project.
    pub fn find_task(&self, key: &str) -> Result<Task> {
        let (slug, id) = parse_task_key(key)?;
        let task = self.get_task(id)?;
        if let Some(slug) = slug
            && slug != task.project
        {
            return Err(Error::NotFound(format!("no task {key} (#{id} is {})", task.key())));
        }
        Ok(task)
    }

    fn check_status(&self, status: &str) -> Result<()> {
        if self.statuses().iter().any(|s| s == status) {
            Ok(())
        } else {
            Err(Error::Invalid(format!("unknown status {status:?}; use one of: {}", self.statuses().join(", "))))
        }
    }

    fn end_of_column(c: &Connection, status: &str, except: i64) -> Result<f64> {
        let max: Option<f64> = c.query_row(
            "SELECT max(position) FROM tasks WHERE status = ?1 AND id <> ?2",
            params![status, except],
            |r| r.get(0),
        )?;
        Ok(max.map_or(1.0, |m| m.floor() + 1.0))
    }

    /// Adds a task at the bottom of todo; a shorthand for tests.
    #[cfg(test)]
    pub fn create_task(&self, project_id: i64, title: &str, description: &str) -> Result<Task> {
        self.create_task_in(project_id, None, title, description)
    }

    /// Adds a task at the bottom of `status`, todo when left out.
    pub fn create_task_in(
        &self,
        project_id: i64,
        status: Option<&str>,
        title: &str,
        description: &str,
    ) -> Result<Task> {
        let title = clean_title(title)?;
        let status = status.unwrap_or(STATUSES[0]);
        self.check_status(status)?;
        let id = self.write(|c| {
            let position = Self::end_of_column(c, status, 0)?;
            c.execute(
                "INSERT INTO tasks (project_id, title, description, status, position) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![project_id, title, description.trim(), status, position],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        self.get_task(id)
    }

    pub fn update_task(&self, id: i64, patch: TaskPatch) -> Result<Task> {
        let title = patch.title.as_deref().map(clean_title).transpose()?;
        let gitlab_mr = patch
            .gitlab_mr
            .as_deref()
            .map(crate::gitlab::normalize_merge_request_url)
            .transpose()
            .map_err(Error::Invalid)?;
        let scheduled_for =
            patch.scheduled_for.as_ref().map(|day| day.as_deref().map(clean_day).transpose()).transpose()?;
        let before = self.get_task(id)?;
        let journey_id = patch.journey_id.clone().unwrap_or(before.journey_id);
        if let Some(journey_id) = journey_id {
            self.get_journey(journey_id)?;
        }
        if let Some(milestone_id) = patch.milestone_id.clone().unwrap_or(before.milestone_id) {
            let milestone = self.get_milestone(milestone_id)?;
            if Some(milestone.journey_id) != journey_id {
                return Err(Error::Invalid("the milestone must belong to the selected journey".into()));
            }
        }
        if let Some(s) = &patch.status {
            self.check_status(s)?;
        }
        self.write(|c| {
            let current = self.get_task(id)?;
            if let Some(title) = title {
                c.execute(
                    &format!("UPDATE tasks SET title = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![title, id],
                )?;
            }
            if let Some(p) = patch.project_id {
                c.execute(
                    &format!("UPDATE tasks SET project_id = ?1, updated_at = {NOW} WHERE id = ?2 AND project_id <> ?1"),
                    params![p, id],
                )?;
            }
            if let Some(human) = patch.human {
                c.execute(
                    &format!("UPDATE tasks SET human = ?1, updated_at = {NOW} WHERE id = ?2 AND human <> ?1"),
                    params![human, id],
                )?;
            }
            if let Some(gitlab_mr) = &gitlab_mr {
                c.execute(
                    &format!("UPDATE tasks SET gitlab_mr = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![gitlab_mr, id],
                )?;
            }
            if let Some(journey_id) = patch.journey_id {
                c.execute(
                    &format!("UPDATE tasks SET journey_id = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![journey_id, id],
                )?;
            }
            if let Some(milestone_id) = patch.milestone_id {
                c.execute(
                    &format!("UPDATE tasks SET milestone_id = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![milestone_id, id],
                )?;
            }
            if let Some(scheduled_for) = &scheduled_for {
                c.execute(
                    &format!("UPDATE tasks SET scheduled_for = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![scheduled_for, id],
                )?;
            }
            if let Some(t) = &patch.tags {
                c.execute(
                    &format!("UPDATE tasks SET tags = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![clean_tags(t).join(","), id],
                )?;
            }
            if let Some(d) = &patch.description {
                c.execute(
                    &format!("UPDATE tasks SET description = ?1, updated_at = {NOW} WHERE id = ?2"),
                    params![d.trim(), id],
                )?;
            }
            if let Some(s) = &patch.status
                && *s != current.status
            {
                let position = Self::end_of_column(c, s, id)?;
                c.execute(
                    &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                    params![s, position, id],
                )?;
            }
            Ok(())
        })?;
        self.get_task(id)
    }

    /// Moves a task to `status`, just above task `before`, or to the bottom when `before` is `None`.
    pub fn move_task(&self, id: i64, status: &str, before: Option<i64>) -> Result<Task> {
        self.check_status(status)?;
        self.write(|c| {
            let current = self.get_task(id)?;
            let before = before.filter(|b| *b != id);
            // The task to go above, with its position, if it's in that column.
            let anchor: Option<(i64, f64)> = match before {
                Some(b) => c
                    .query_row("SELECT position FROM tasks WHERE id = ?1 AND status = ?2", params![b, status], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .map(|p| (b, p)),
                None => None,
            };
            let position = match anchor {
                None => Self::end_of_column(c, status, id)?,
                Some((before, mut next)) => {
                    let prev_of = |next: f64| -> Result<Option<f64>> {
                        Ok(c.query_row(
                            "SELECT max(position) FROM tasks WHERE status = ?1 AND position < ?2 AND id <> ?3",
                            params![status, next, id],
                            |r| r.get(0),
                        )?)
                    };
                    let mut prev = prev_of(next)?;
                    if prev.is_some_and(|p| next - p < 1e-9) {
                        // Out of room between two neighbours: renumber the column 1, 2, 3...
                        Self::renumber(c, status)?;
                        next = c.query_row("SELECT position FROM tasks WHERE id = ?1", [before], |r| r.get(0))?;
                        prev = prev_of(next)?;
                    }
                    match prev {
                        Some(p) => (p + next) / 2.0,
                        None => next - 1.0,
                    }
                }
            };
            let touched = if current.status != status { format!(", updated_at = {NOW}") } else { String::new() };
            c.execute(
                &format!("UPDATE tasks SET status = ?1, position = ?2{touched} WHERE id = ?3"),
                params![status, position, id],
            )?;
            Ok(())
        })?;
        self.get_task(id)
    }

    fn renumber(c: &Connection, status: &str) -> Result<()> {
        let ids: Vec<i64> = c
            .prepare("SELECT id FROM tasks WHERE status = ?1 ORDER BY position")?
            .query_map([status], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut update = c.prepare("UPDATE tasks SET position = ?1 WHERE id = ?2")?;
        for (i, id) in ids.iter().enumerate() {
            update.execute(params![(i + 1) as f64, id])?;
        }
        Ok(())
    }

    /// Takes the top task of todo that isn't for a person, and moves it to the bottom
    /// of doing.
    pub fn next_task(&self, project_id: Option<i64>) -> Result<Option<Task>> {
        let [from, to, ..] = STATUSES;
        let id = self.write(|c| {
            let id: Option<i64> = c
                .query_row(
                    "SELECT id FROM tasks WHERE status = ?1 AND human = 0 AND (?2 IS NULL OR project_id = ?2) \
                     ORDER BY position LIMIT 1",
                    params![from, project_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                let position = Self::end_of_column(c, to, id)?;
                c.execute(
                    &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                    params![to, position, id],
                )?;
            }
            Ok(id)
        })?;
        id.map(|id| self.get_task(id)).transpose()
    }

    /// Deletes a task and its comments; returns the task as it was.
    pub fn delete_task(&self, id: i64) -> Result<Task> {
        let task = self.get_task(id)?;
        self.conn.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        Ok(task)
    }

    // Comments

    pub fn list_comments(&self, task_id: i64) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, task_id, author, body, created_at FROM comments WHERE task_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([task_id], Self::comment_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn comment_row(r: &Row) -> rusqlite::Result<Comment> {
        Ok(Comment { id: r.get(0)?, task_id: r.get(1)?, author: r.get(2)?, body: r.get(3)?, created_at: r.get(4)? })
    }

    /// Adds a comment, with the images (files already in `images/`) attached to it.
    /// A comment needs text, an image, or both.
    pub fn add_comment(&self, task_id: i64, author: &str, body: &str, images: &[String]) -> Result<Comment> {
        let body = body.trim();
        if body.is_empty() && images.is_empty() {
            return Err(Error::Invalid("comment body is empty".into()));
        }
        if let Some(bad) = images.iter().find(|f| !valid_image_file(f)) {
            return Err(Error::Invalid(format!("{bad:?} is not an image Kanzo saved")));
        }
        let author = match author.trim() {
            "" => "me",
            a => a,
        };
        self.get_task(task_id)?;
        let id = self.write(|c| {
            c.execute(
                "INSERT INTO comments (task_id, author, body) VALUES (?1, ?2, ?3)",
                params![task_id, author, body],
            )?;
            let id = c.last_insert_rowid();
            let mut insert = c.prepare("INSERT INTO images (task_id, comment_id, file) VALUES (?1, ?2, ?3)")?;
            for file in images {
                insert.execute(params![task_id, id, file])?;
            }
            Ok(id)
        })?;
        self.get_comment(id)
    }

    fn get_comment(&self, id: i64) -> Result<Comment> {
        self.conn
            .query_row(
                "SELECT id, task_id, author, body, created_at FROM comments WHERE id = ?1",
                [id],
                Self::comment_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no comment {id}")))
    }

    /// Deletes a comment; returns it as it was.
    pub fn delete_comment(&self, id: i64) -> Result<Comment> {
        let comment = self.get_comment(id)?;
        self.conn.execute("DELETE FROM comments WHERE id = ?1", [id])?;
        Ok(comment)
    }

    // Images

    /// Attaches an image, already saved in `images/`, to a task.
    pub fn add_image(&self, task_id: i64, file: &str) -> Result<Image> {
        if !valid_image_file(file) {
            return Err(Error::Invalid(format!("{file:?} is not an image Kanzo saved")));
        }
        self.get_task(task_id)?;
        self.conn.execute("INSERT INTO images (task_id, file) VALUES (?1, ?2)", params![task_id, file])?;
        Ok(Image { id: self.conn.last_insert_rowid(), task_id, comment_id: None, file: file.into() })
    }

    /// The images of a task and of its comments, oldest first.
    pub fn list_images(&self, task_id: i64) -> Result<Vec<Image>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, task_id, comment_id, file FROM images WHERE task_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([task_id], Self::image_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn image_row(r: &Row) -> rusqlite::Result<Image> {
        Ok(Image { id: r.get(0)?, task_id: r.get(1)?, comment_id: r.get(2)?, file: r.get(3)? })
    }

    /// Removes an image from its task or comment; returns it as it was.
    pub fn delete_image(&self, id: i64) -> Result<Image> {
        let image = self
            .conn
            .query_row("SELECT id, task_id, comment_id, file FROM images WHERE id = ?1", [id], Self::image_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no image {id}")))?;
        self.conn.execute("DELETE FROM images WHERE id = ?1", [id])?;
        Ok(image)
    }

    /// The image files attached to one image, comment, task (with its comments) or project.
    pub fn image_files(&self, of: ImagesOf) -> Result<Vec<String>> {
        let (sql, id) = match of {
            ImagesOf::Image(id) => ("SELECT file FROM images WHERE id = ?1", id),
            ImagesOf::Comment(id) => ("SELECT DISTINCT file FROM images WHERE comment_id = ?1", id),
            ImagesOf::Task(id) => ("SELECT DISTINCT file FROM images WHERE task_id = ?1", id),
            ImagesOf::Project(id) => {
                ("SELECT DISTINCT i.file FROM images i JOIN tasks t ON t.id = i.task_id WHERE t.project_id = ?1", id)
            }
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Of `files`, those no task or comment uses any more.
    pub fn unused_images(&self, files: Vec<String>) -> Result<Vec<String>> {
        let mut used = self.conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM images WHERE file = ?1)")?;
        let mut unused = Vec::new();
        for file in files {
            if !used.query_row([&file], |r| r.get::<_, bool>(0))? {
                unused.push(file);
            }
        }
        Ok(unused)
    }
}

/// Whose images `Store::image_files` lists.
pub enum ImagesOf {
    Image(i64),
    Comment(i64),
    Task(i64),
    Project(i64),
}

/// The name Kanzo gives a saved image: 16 hex digits and its type, like `6fec521a01297ab5.png`.
pub fn valid_image_file(file: &str) -> bool {
    match file.split_once('.') {
        Some((hash, ext)) => {
            hash.len() == 16
                && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                && ["png", "jpg", "gif", "webp"].contains(&ext)
        }
        None => false,
    }
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `2026-10-25T09:30:00Z` -> `2026-10-25 09:30 UTC`.
pub fn short_time(ts: &str) -> String {
    match (ts.get(..10), ts.get(11..16)) {
        (Some(d), Some(t)) => format!("{d} {t} UTC"),
        _ => ts.to_string(),
    }
}

/// `" Bug, UI,bug ,, needs  review"` -> `["bug", "ui", "needs review"]`.
fn clean_tags(input: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in input.split(',') {
        let tag = squash(tag).to_lowercase();
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

fn clean_repo(repo: &str) -> Result<String> {
    repo_from_url(repo).ok_or_else(|| Error::Invalid(format!("repo {:?} should look like owner/repo", repo.trim())))
}

/// `"  a   b "` -> `"a b"`: words joined by single spaces.
fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn clean_title(title: &str) -> Result<String> {
    let title = squash(title);
    if title.is_empty() {
        return Err(Error::Invalid("task title is empty".into()));
    }
    Ok(title)
}

/// A user-facing local calendar day. We deliberately do not convert it to UTC:
/// "today" must mean the same thing in the browser and the local data file.
fn clean_day(day: &str) -> Result<String> {
    let day = day.trim();
    let bytes = day.as_bytes();
    let valid = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes.iter().enumerate().all(|(i, byte)| matches!(i, 4 | 7) || byte.is_ascii_digit());
    if valid { Ok(day.to_owned()) } else { Err(Error::Invalid("day must use YYYY-MM-DD".into())) }
}

/// Splits `api#12`, `#12` or `12` into an optional project slug and the task id.
fn parse_task_key(key: &str) -> Result<(Option<String>, i64)> {
    let key = key.trim();
    let (slug, num) = match key.rsplit_once('#') {
        Some((s, n)) => (Some(s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()), n),
        None => (None, key),
    };
    let id = num
        .trim()
        .parse::<i64>()
        .map_err(|_| Error::Invalid(format!("{key:?} is not a task id; use a form like api#12 or 12")))?;
    Ok((slug, id))
}

/// Normalises a GitHub repo given as `owner/repo` or as any git remote URL
/// (`https://github.com/owner/repo.git`, `git@github.com:owner/repo`, ...) to lowercase `owner/repo`.
/// A browser URL like `https://github.com/owner/repo/tree/main` gives `owner/repo` too.
pub fn repo_from_url(input: &str) -> Option<String> {
    let s = input.trim();
    let (path, bare) = if let Some((_, rest)) = s.split_once("://") {
        // scheme://[user@]host[:port]/owner/repo...
        (rest.split_once('/')?.1, false)
    } else if let Some((_, rest)) = s.split_once(':') {
        // user@host:owner/repo
        (rest, false)
    } else {
        (s, true)
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 || (bare && parts.len() != 2) {
        return None;
    }
    let owner = parts[0];
    let repo = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
    let ok = |p: &str| p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok(owner) && ok(repo)).then(|| format!("{owner}/{repo}").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("kanzo.db"), None).unwrap();
        store.migrate().unwrap();
        (dir, store)
    }

    fn titles(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn migrate_is_idempotent() {
        let (_dir, s) = store();
        s.migrate().unwrap();
        let v: String =
            s.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len().to_string());
    }

    #[test]
    fn planning_links_a_task_to_its_milestone() {
        let (_dir, s) = store();
        let area = s.create_life_area("learning", "Learning", Some("purple")).unwrap();
        let journey = s.create_journey(area.id, "Learn Rust", "Build reliable tools", None).unwrap();
        let milestone = s.create_milestone(journey.id, "Finish storage module", None).unwrap();
        let project = s.create_project("kanzo", "Kanzo", None).unwrap();
        assert_eq!(s.set_project_life_area(&project.slug, Some(area.id)).unwrap().life_area_id, Some(area.id));
        let task = s.create_task(project.id, "Write migration", "").unwrap();
        let task = s
            .update_task(
                task.id,
                TaskPatch {
                    journey_id: Some(Some(journey.id)),
                    milestone_id: Some(Some(milestone.id)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!((task.journey_id, task.milestone_id), (Some(journey.id), Some(milestone.id)));
        let summary = s.list_journeys().unwrap();
        assert_eq!((summary[0].milestones_done, summary[0].milestones_total), (0, 1));
        let wrong_area = s.create_life_area("health", "Health", None).unwrap();
        let other = s.create_journey(wrong_area.id, "Run", "", None).unwrap();
        assert!(s.update_task(task.id, TaskPatch { journey_id: Some(Some(other.id)), ..Default::default() }).is_err());
        let cleared = s
            .update_task(task.id, TaskPatch { journey_id: Some(None), milestone_id: Some(None), ..Default::default() })
            .unwrap();
        assert_eq!((cleared.journey_id, cleared.milestone_id), (None, None));
    }

    #[test]
    fn daily_priorities_are_ordered_and_scheduled_tasks_can_move_days() {
        let (_dir, s) = store();
        let project = s.create_project("personal", "Personal", None).unwrap();
        let first = s.create_task(project.id, "Write weekly plan", "").unwrap();
        let second = s.create_task(project.id, "Take a walk", "").unwrap();
        let third = s.create_task(project.id, "Call family", "").unwrap();
        let fourth = s.create_task(project.id, "Read", "").unwrap();
        let day = "2026-11-01";
        let priorities = s.set_daily_priorities(day, &[second.id, first.id, third.id]).unwrap();
        assert_eq!(titles(&priorities), ["Take a walk", "Write weekly plan", "Call family"]);
        assert!(s.set_daily_priorities(day, &[first.id, second.id, third.id, fourth.id]).is_err());
        let moved =
            s.update_task(first.id, TaskPatch { scheduled_for: Some(Some(day.into())), ..Default::default() }).unwrap();
        assert_eq!(moved.scheduled_for.as_deref(), Some(day));
        assert_eq!(titles(&s.list_scheduled_tasks(day).unwrap()), ["Write weekly plan"]);
        let moved = s
            .update_task(first.id, TaskPatch { scheduled_for: Some(Some("2026-11-02".into())), ..Default::default() })
            .unwrap();
        assert_eq!(moved.scheduled_for.as_deref(), Some("2026-11-02"));
        assert!(s.list_scheduled_tasks(day).unwrap().is_empty());
        assert!(
            s.update_task(first.id, TaskPatch { scheduled_for: Some(Some("tomorrow".into())), ..Default::default() })
                .is_err()
        );
    }

    #[test]
    fn weekly_outcomes_and_carry_forward_are_kept_separate_from_this_week() {
        let (_dir, s) = store();
        let project = s.create_project("personal", "Personal", None).unwrap();
        let past = s.create_task(project.id, "Unfinished review", "").unwrap();
        let current = s.create_task(project.id, "Plan Monday", "").unwrap();
        s.update_task(past.id, TaskPatch { scheduled_for: Some(Some("2026-10-30".into())), ..Default::default() })
            .unwrap();
        s.update_task(current.id, TaskPatch { scheduled_for: Some(Some("2026-11-02".into())), ..Default::default() })
            .unwrap();
        let saved = s
            .set_weekly_outcomes("2026-11-02", &["Plan calmly".into(), "Move three times".into(), "Plan calmly".into()])
            .unwrap();
        assert_eq!(saved, ["Plan calmly", "Move three times"]);
        assert_eq!(titles(&s.list_tasks_scheduled_between("2026-11-02", "2026-11-08").unwrap()), ["Plan Monday"]);
        assert_eq!(titles(&s.list_carry_forward_tasks("2026-11-02").unwrap()), ["Unfinished review"]);
    }

    #[test]
    fn habit_checks_are_saved_per_day_and_can_be_undone() {
        let (_dir, s) = store();
        let habit = s.create_habit("Read 20 min").unwrap();
        assert_eq!(s.list_habits().unwrap()[0].title, "Read 20 min");
        s.set_habit_check(habit.id, "2026-11-01", true).unwrap();
        assert_eq!(s.list_habit_checks("2026-10-26", "2026-11-01").unwrap().len(), 1);
        s.set_habit_check(habit.id, "2026-11-01", false).unwrap();
        assert!(s.list_habit_checks("2026-10-26", "2026-11-01").unwrap().is_empty());
        assert!(s.set_habit_check(habit.id, "sunday", true).is_err());
    }

    #[test]
    fn keeps_only_the_newest_done_tasks() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        s.create_task(p.id, "open", "").unwrap();
        for title in ["d1", "d2", "d3", "d4"] {
            let t = s.create_task(p.id, title, "").unwrap();
            s.move_task(t.id, "done", None).unwrap();
        }
        let (tasks, hidden) = s.list_tasks_keeping_done(None, None, Some(2)).unwrap();
        assert_eq!((titles(&tasks), hidden), (vec!["open", "d3", "d4"], 2));
        let (tasks, hidden) = s.list_tasks_keeping_done(Some(p.id), Some("done"), Some(0)).unwrap();
        assert_eq!((tasks.len(), hidden), (0, 4));
        assert_eq!(s.list_tasks_keeping_done(None, Some("todo"), Some(0)).unwrap().1, 0);
        let (all, hidden) = s.list_tasks_keeping_done(None, None, None).unwrap();
        assert_eq!((titles(&all), hidden), (vec!["open", "d1", "d2", "d3", "d4"], 0));
    }

    #[test]
    fn opens_a_board_from_a_newer_kanzo_when_it_can() {
        let (_dir, s) = store();
        let set = |version: usize, compatible: usize| {
            s.conn
                .execute_batch(&format!(
                    "UPDATE meta SET value = '{version}' WHERE key = 'schema_version';
                     UPDATE meta SET value = '{compatible}' WHERE key = 'compatible';"
                ))
                .unwrap();
        };
        let newer = MIGRATIONS.len() + 1;
        // A newer Kanzo only added things: this one goes on using the board, untouched.
        set(newer, MIGRATIONS.len());
        s.migrate().unwrap();
        s.create_project("api", "", None).unwrap();
        let v: String =
            s.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, newer.to_string());
        // A change this version can't live with: it says how to update.
        set(newer, newer);
        assert!(s.migrate().unwrap_err().to_string().contains("kanzo update"));
    }

    #[test]
    fn projects() {
        let (_dir, s) = store();
        let p = s.create_project("API", "API server", Some("https://github.com/Me/Api.git")).unwrap();
        assert_eq!((p.slug.as_str(), p.repos.as_slice()), ("api", ["me/api".to_string()].as_slice()));
        assert_eq!(s.find_project("me/api").unwrap().id, p.id);
        assert_eq!(s.project_by_repo("git@github.com:me/api.git").unwrap().unwrap().id, p.id);
        assert!(s.create_project("api", "", None).unwrap_err().to_string().contains("already exists"));
        assert!(s.create_project("web", "", Some("me/api")).unwrap_err().to_string().contains("already linked"));
        assert!(s.create_project("bad slug", "", None).is_err());
        let web = s.create_project("web", "", None).unwrap();
        assert_eq!((web.name.as_str(), web.color.as_deref()), ("web", None));
        assert_eq!(s.set_project_color("web", "teal").unwrap().color.as_deref(), Some("teal"));
        assert!(s.set_project_color("web", "#ff0000").is_err());
        let renamed = s.rename_project("web", "  Web   site ").unwrap();
        assert_eq!((renamed.slug.as_str(), renamed.name.as_str()), ("web", "Web site"));
        assert!(s.rename_project("web", "  ").is_err());
        assert!(s.find_project("nope").unwrap_err().to_string().contains("api, web"));
    }

    #[test]
    fn many_repos_per_project() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("me/api")).unwrap();
        s.create_project("web", "", None).unwrap();
        assert_eq!(s.link_repo("api", "https://github.com/Me/API-docs.git").unwrap().repos, ["me/api", "me/api-docs"]);
        assert_eq!(s.find_project("me/api-docs").unwrap().id, api.id);
        assert_eq!(s.link_repo("api", "me/api").unwrap().repos.len(), 2, "linking twice changes nothing");
        assert!(s.link_repo("web", "me/api-docs").unwrap_err().to_string().contains("already linked to project api"));
        assert!(s.link_repo("api", "not a repo").is_err());
        assert_eq!(s.unlink_repo("api", "me/api").unwrap().repos, ["me/api-docs"]);
        assert!(s.project_by_repo("me/api").unwrap().is_none());
        assert!(s.unlink_repo("api", "me/api").is_err());
        // A deleted project frees its repos.
        s.delete_project("api").unwrap();
        assert_eq!(s.link_repo("web", "me/api-docs").unwrap().repos, ["me/api-docs"]);
    }

    #[test]
    fn search() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let login = s.create_task(api.id, "Fix login", "Users loop on /login").unwrap();
        let old = s.create_task(web.id, "Login page copy", "").unwrap();
        s.update_task(old.id, TaskPatch { status: Some("done".into()), ..Default::default() }).unwrap();
        let other = s.create_task(web.id, "Pricing 100%", "").unwrap();
        s.update_task(other.id, TaskPatch { tags: Some("ui".into()), ..Default::default() }).unwrap();
        let titles = |q: &str| s.search(q, 10).unwrap().into_iter().map(|t| t.title).collect::<Vec<_>>();
        assert_eq!(titles("LOGIN"), ["Fix login", "Login page copy"], "any project, open first, done too");
        assert_eq!(titles("login api"), ["Fix login"], "every word must match, the project too");
        assert_eq!(titles(&format!("web#{}", other.id)), ["Pricing 100%"], "by key");
        assert_eq!(titles("ui"), ["Pricing 100%"], "by tag");
        assert_eq!(titles("100%"), ["Pricing 100%"], "% is a plain character");
        assert!(titles("nothing-like-this").is_empty());
        assert_eq!(s.search("", 10).unwrap().len(), 3, "no words: the latest changes");
        assert_eq!(s.search("login", 1).unwrap()[0].id, login.id);
    }

    #[test]
    fn images() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "Bug", "").unwrap();
        let shot = "0123456789abcdef.png".to_string();
        let on_task = s.add_image(t.id, &shot).unwrap();
        assert!(s.add_image(t.id, "../../etc/passwd").is_err());
        assert!(s.add_image(t.id + 1, &shot).is_err());
        // A comment can be only images.
        let c = s.add_comment(t.id, "me", "", &["fedcba9876543210.jpg".into(), shot.clone()]).unwrap();
        assert!(s.add_comment(t.id, "me", "x", &["evil.svg".into()]).is_err());
        let all: Vec<_> = s.list_images(t.id).unwrap().into_iter().map(|i| (i.comment_id, i.file)).collect();
        assert_eq!(
            all,
            [(None, shot.clone()), (Some(c.id), "fedcba9876543210.jpg".into()), (Some(c.id), shot.clone())]
        );
        assert_eq!(s.image_files(ImagesOf::Task(t.id)).unwrap(), [shot.clone(), "fedcba9876543210.jpg".into()]);
        assert_eq!(s.image_files(ImagesOf::Project(p.id)).unwrap().len(), 2);
        // The same file twice: removing one attachment leaves it in use.
        s.delete_image(on_task.id).unwrap();
        assert!(s.unused_images(vec![shot.clone()]).unwrap().is_empty());
        let files = s.image_files(ImagesOf::Comment(c.id)).unwrap();
        s.delete_comment(c.id).unwrap();
        assert!(s.list_images(t.id).unwrap().is_empty());
        assert_eq!(s.unused_images(files).unwrap(), ["fedcba9876543210.jpg".to_string(), shot.clone()]);
        assert!(valid_image_file("6fec521a01297ab5.webp"));
        assert!(!valid_image_file("6FEC521A01297AB5.png") && !valid_image_file("6fec521a01297ab5.svg"));
    }

    #[test]
    fn migration_moves_repos_to_their_own_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        {
            // A board at schema version 3, from before projects could have several repos.
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..3] {
                c.execute_batch(sql).unwrap();
            }
            c.execute_batch(
                "INSERT INTO meta VALUES ('schema_version', '3');
                 INSERT INTO projects (slug, name, repo) VALUES ('api', 'api', 'me/api'), ('web', 'web', NULL);",
            )
            .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        assert_eq!(s.find_project("api").unwrap().repos, ["me/api"]);
        assert!(s.find_project("web").unwrap().repos.is_empty());
        assert_eq!(s.find_project("me/api").unwrap().slug, "api");
    }

    #[test]
    fn task_lifecycle() {
        let (_dir, s) = store();
        let p = s.create_project("api", "API", None).unwrap();
        let t = s.create_task(p.id, "  Fix   login ", "Steps:\n1. log in\n").unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str(), t.key()), ("Fix login", "todo", format!("api#{}", t.id)));
        assert_eq!(s.find_task(&t.key()).unwrap().id, t.id);
        assert!(s.find_task(&format!("web#{}", t.id)).is_err());
        let t = s
            .update_task(
                t.id,
                TaskPatch {
                    gitlab_mr: Some("https://gitlab.com/team/platform/-/merge_requests/42?tab=diffs".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(t.gitlab_mr.as_deref(), Some("https://gitlab.com/team/platform/-/merge_requests/42"));
        let t = s.update_task(t.id, TaskPatch { gitlab_mr: Some(String::new()), ..Default::default() }).unwrap();
        assert!(t.gitlab_mr.is_none());
        assert!(
            s.update_task(t.id, TaskPatch { gitlab_mr: Some("https://example.com/nope".into()), ..Default::default() })
                .is_err()
        );

        let t = s
            .update_task(
                t.id,
                TaskPatch { status: Some("review".into()), title: Some("Fix login bug".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("Fix login bug", "review"));
        assert!(s.update_task(t.id, TaskPatch { status: Some("nope".into()), ..Default::default() }).is_err());
        assert!(s.update_task(t.id, TaskPatch { title: Some("  ".into()), ..Default::default() }).is_err());
        let web = s.create_project("web", "", None).unwrap();
        let moved = s.update_task(t.id, TaskPatch { project_id: Some(web.id), ..Default::default() }).unwrap();
        assert_eq!((moved.key(), moved.status.as_str()), (format!("web#{}", t.id), "review"));
        let t = s.update_task(t.id, TaskPatch { project_id: Some(p.id), ..Default::default() }).unwrap();

        s.add_comment(t.id, "claude-code", "Done, see PR", &[]).unwrap();
        s.add_comment(t.id, "", "thanks", &[]).unwrap();
        let comments = s.list_comments(t.id).unwrap();
        assert_eq!(comments.iter().map(|c| c.author.as_str()).collect::<Vec<_>>(), ["claude-code", "me"]);
        assert!(s.add_comment(t.id, "me", " ", &[]).is_err());
        assert_eq!(s.get_task(t.id).unwrap().comments, 2);
        let tagged =
            s.update_task(t.id, TaskPatch { tags: Some(" Bug, UI,bug ,, needs  review".into()), ..Default::default() });
        assert_eq!(tagged.unwrap().tags, ["bug", "ui", "needs review"]);
        assert!(
            s.update_task(t.id, TaskPatch { tags: Some(" , ".into()), ..Default::default() }).unwrap().tags.is_empty()
        );
        let gone = s.add_comment(t.id, "me", "typo", &[]).unwrap();
        assert_eq!(s.delete_comment(gone.id).unwrap().body, "typo");
        assert!(s.delete_comment(gone.id).is_err());
        assert_eq!(s.get_task(t.id).unwrap().comments, 2);
        assert_eq!(s.find_project("api").unwrap().open, 1);

        s.delete_task(t.id).unwrap();
        assert!(s.get_task(t.id).is_err());
        let orphans: i64 = s.conn.query_row("SELECT count(*) FROM comments", [], |r| r.get(0)).unwrap();
        assert_eq!(orphans, 0);
        // Ids are never reused, so api#12 always means the same task.
        let t2 = s.create_task(p.id, "Another", "").unwrap();
        assert!(t2.id > t.id);
    }

    #[test]
    fn delete_project_removes_its_tasks_and_comments() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("me/api")).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let t = s.create_task(api.id, "a", "").unwrap();
        s.add_comment(t.id, "me", "hi", &[]).unwrap();
        let kept = s.create_task(web.id, "b", "").unwrap();
        s.add_comment(kept.id, "me", "hello", &[]).unwrap();

        assert_eq!(s.delete_project("api").unwrap().id, api.id);
        assert!(s.find_project("api").is_err());
        assert!(s.get_task(t.id).is_err());
        let count = |table: &str| -> i64 {
            s.conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        assert_eq!((count("projects"), count("tasks"), count("comments")), (1, 1, 1));
        assert!(s.delete_project("api").is_err());
        // The slug and repo are free again.
        s.create_project("api", "", Some("me/api")).unwrap();
    }

    #[test]
    fn ordering_and_moves() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let a = s.create_task(p.id, "a", "").unwrap();
        let b = s.create_task(p.id, "b", "").unwrap();
        let c = s.create_task(p.id, "c", "").unwrap();
        assert_eq!(titles(&s.list_tasks(Some(p.id), None).unwrap()), ["a", "b", "c"]);

        s.move_task(c.id, "todo", Some(a.id)).unwrap();
        assert_eq!(titles(&s.list_tasks(None, Some("todo")).unwrap()), ["c", "a", "b"]);
        s.move_task(c.id, "todo", None).unwrap();
        assert_eq!(titles(&s.list_tasks(None, Some("todo")).unwrap()), ["a", "b", "c"]);
        s.move_task(a.id, "doing", None).unwrap();
        s.move_task(b.id, "doing", Some(a.id)).unwrap();
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["c", "b", "a"]);

        // Many inserts at the same spot run out of float room and trigger a renumber.
        let mut moving = [b.id, c.id];
        for _ in 0..80 {
            s.move_task(moving[0], "doing", Some(a.id)).unwrap();
            moving.swap(0, 1);
        }
        assert_eq!(titles(&s.list_tasks(None, Some("doing")).unwrap()).last(), Some(&"a"));
        assert!(s.move_task(a.id, "nope", None).is_err());
    }

    #[test]
    fn next_task_takes_top_todo() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        s.create_task(web.id, "web first", "").unwrap();
        // At the top of api's todo, but for a person: agents never get it.
        let human = s.create_task(api.id, "Call the bank", "").unwrap();
        assert!(s.update_task(human.id, TaskPatch { human: Some(true), ..Default::default() }).unwrap().human);
        s.create_task(api.id, "api first", "").unwrap();
        s.create_task(api.id, "api second", "").unwrap();

        let t = s.next_task(Some(api.id)).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("api first", "doing"));
        assert_eq!(s.next_task(None).unwrap().unwrap().title, "web first");
        assert_eq!(s.next_task(Some(api.id)).unwrap().unwrap().title, "api second");
        assert!(s.next_task(Some(api.id)).unwrap().is_none());
        assert_eq!(s.get_task(human.id).unwrap().status, "todo");
    }

    #[test]
    fn backlog_is_a_column_agents_skip() {
        let (dir, s) = store();
        assert_eq!(s.statuses(), STATUSES);
        s.set_backlog(true).unwrap();
        let s = Store::open(&dir.path().join("kanzo.db"), None).unwrap();
        assert_eq!(s.statuses(), ["backlog", "todo", "doing", "review", "done"]);
        let api = s.create_project("api", "", None).unwrap();
        let backlog = s.create_task_in(api.id, Some("backlog"), "later", "").unwrap();
        assert_eq!(backlog.status, "backlog");
        assert!(s.create_task_in(api.id, Some("nope"), "x", "").is_err());
        assert_eq!(s.create_task(api.id, "now", "").unwrap().status, "todo");

        let t = s.next_task(None).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("now", "doing"));
        assert!(s.next_task(None).unwrap().is_none());
        assert_eq!(s.get_task(backlog.id).unwrap().status, "backlog");

        // Hiding it would hide its tasks.
        assert!(s.set_backlog(false).is_err());
        s.delete_task(backlog.id).unwrap();
        s.set_backlog(false).unwrap();
    }

    #[test]
    fn change_counter_moves_on_every_write() {
        let (_dir, s) = store();
        let c0 = s.changes().unwrap();
        let p = s.create_project("api", "", None).unwrap();
        let c1 = s.changes().unwrap();
        let t = s.create_task(p.id, "x", "").unwrap();
        let c2 = s.changes().unwrap();
        s.add_comment(t.id, "me", "hi", &[]).unwrap();
        let c3 = s.changes().unwrap();
        assert!(c0 < c1 && c1 < c2 && c2 < c3);
    }

    #[test]
    fn task_keys() {
        assert_eq!(parse_task_key("api#12").unwrap(), (Some("api".into()), 12));
        assert_eq!(parse_task_key("#12").unwrap(), (None, 12));
        assert_eq!(parse_task_key(" 12 ").unwrap(), (None, 12));
        assert!(parse_task_key("api#x").is_err());
    }

    #[test]
    fn repos() {
        for url in [
            "owner/repo",
            "Owner/Repo",
            "https://github.com/owner/repo",
            "https://github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "git@github-work:owner/repo.git",
            "https://github.com/owner/repo/",
            "https://github.com/owner/repo/tree/main",
            "https://github.com/owner/repo/pull/3",
            // Gitea, or any other host: only owner/repo counts.
            "https://gitea.example.com/owner/repo.git",
            "git@gitea.example.com:owner/repo.git",
            "ssh://git@gitea.example.com:2222/owner/repo.git",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("owner/repo"), "{url}");
        }
        assert_eq!(repo_from_url("repo"), None);
        assert_eq!(repo_from_url("a/b/c"), None);
        assert_eq!(repo_from_url("own er/repo"), None);
    }
}
