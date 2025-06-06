/// Postgres based background job queue provider
use std::{
    collections::HashMap, future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc,
    time::Duration,
};

use super::{BackgroundWorker, JobStatus, Queue};
use crate::{config::PostgresQueueConfig, Error, Result};
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
pub use sqlx::PgPool;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions, PgRow},
    ConnectOptions, Row,
};
use std::fmt::Write;
use tokio::{task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace};
use ulid::Ulid;
type JobId = String;
type JobData = JsonValue;

type JobHandler = Box<
    dyn Fn(
            JobId,
            JobData,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), crate::Error>> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Job {
    pub id: JobId,
    pub name: String,
    #[serde(rename = "task_data")]
    pub data: JobData,
    pub status: JobStatus,
    pub run_at: DateTime<Utc>,
    pub interval: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub tags: Option<Vec<String>>,
}

pub struct JobRegistry {
    handlers: Arc<HashMap<String, JobHandler>>,
}

impl JobRegistry {
    /// Creates a new `JobRegistry`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handlers: Arc::new(HashMap::new()),
        }
    }

    /// Registers a job handler with the provided name.
    /// # Errors
    /// Fails if cannot register worker
    pub fn register_worker<Args, W>(&mut self, name: String, worker: W) -> Result<()>
    where
        Args: Send + Serialize + Sync + 'static,
        W: BackgroundWorker<Args> + 'static,
        for<'de> Args: Deserialize<'de>,
    {
        let worker = Arc::new(worker);
        let wrapped_handler = move |_job_id: String, job_data: JobData| {
            let w = worker.clone();

            Box::pin(async move {
                let args = serde_json::from_value::<Args>(job_data);
                match args {
                    Ok(args) => {
                        // Wrap the perform call in catch_unwind to handle panics
                        match AssertUnwindSafe(w.perform(args)).catch_unwind().await {
                            Ok(result) => result,
                            Err(panic) => {
                                let panic_msg = panic
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| panic.downcast_ref::<&str>().copied())
                                    .unwrap_or("Unknown panic occurred");
                                error!(err = panic_msg, "worker panicked");
                                Err(Error::string(panic_msg))
                            }
                        }
                    }
                    Err(err) => Err(err.into()),
                }
            }) as Pin<Box<dyn Future<Output = Result<(), crate::Error>> + Send>>
        };

        Arc::get_mut(&mut self.handlers)
            .ok_or_else(|| Error::string("cannot register worker"))?
            .insert(name, Box::new(wrapped_handler));
        Ok(())
    }

    /// Returns a reference to the job handlers.
    #[must_use]
    pub fn handlers(&self) -> &Arc<HashMap<String, JobHandler>> {
        &self.handlers
    }

    /// Runs the job handlers with the provided number of workers.
    #[must_use]
    pub fn run(
        &self,
        pool: &PgPool,
        opts: &RunOpts,
        token: &CancellationToken,
        tags: &[String],
    ) -> Vec<JoinHandle<()>> {
        let mut jobs = Vec::new();

        let interval = opts.poll_interval_sec;
        for idx in 0..opts.num_workers {
            let handlers = self.handlers.clone();
            let worker_token = token.clone(); // Clone token for this worker
            let worker_tags = tags.to_vec();

            let pool = pool.clone();
            let job = tokio::spawn(async move {
                loop {
                    // Check for cancellation before potentially blocking on dequeue
                    if worker_token.is_cancelled() {
                        trace!(worker_id = idx, "Cancellation received, stopping worker");
                        break;
                    }
                    trace!(
                        pool_size = pool.num_idle(),
                        worker_id = idx,
                        "Connection pool stats"
                    );
                    let job_opt = match dequeue(&pool, &worker_tags).await {
                        Ok(t) => t,
                        Err(err) => {
                            error!(error = %err, "Failed to fetch job from queue");
                            None
                        }
                    };

                    if let Some(job) = job_opt {
                        debug!(job_id = %job.id, job_name = %job.name, "Processing job");
                        if let Some(handler) = handlers.get(&job.name) {
                            match handler(job.id.clone(), job.data.clone()).await {
                                Ok(()) => {
                                    if let Err(err) =
                                        complete_job(&pool, &job.id, job.interval).await
                                    {
                                        error!(
                                            error = %err,
                                            job_id = %job.id,
                                            job_name = %job.name,
                                            "Failed to mark job as completed"
                                        );
                                    } else {
                                        debug!(job_id = %job.id, "Job completed successfully");
                                    }
                                }
                                Err(err) => {
                                    if let Err(fail_err) = fail_job(&pool, &job.id, &err).await {
                                        error!(
                                            error = %fail_err,
                                            job_id = %job.id,
                                            job_name = %job.name,
                                            "Failed to mark job as failed"
                                        );
                                    } else {
                                        debug!(job_id = %job.id, error = %err, "Job execution failed");
                                    }
                                }
                            }
                        } else {
                            error!(job_name = %job.name, "No handler registered for job");
                        }
                    } else {
                        // Use tokio::select! to wait for interval or cancellation
                        tokio::select! {
                            biased;
                            () = worker_token.cancelled() => {
                                trace!(worker_id = idx, "Cancellation received during sleep, stopping worker");
                                break;
                            }
                            () = sleep(Duration::from_secs(interval.into())) => {
                                // Interval elapsed, continue loop
                            }
                        }
                    }
                }
            });

            jobs.push(job);
        }

        jobs
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

async fn connect(cfg: &PostgresQueueConfig) -> Result<PgPool> {
    let mut conn_opts: PgConnectOptions = cfg.uri.parse()?;
    if !cfg.enable_logging {
        conn_opts = conn_opts.disable_statement_logging();
    }
    let pool = PgPoolOptions::new()
        .min_connections(cfg.min_connections)
        .max_connections(cfg.max_connections)
        .idle_timeout(Duration::from_millis(cfg.idle_timeout))
        .acquire_timeout(Duration::from_millis(cfg.connect_timeout))
        .connect_with(conn_opts)
        .await?;
    Ok(pool)
}

/// Initialize job tables
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn initialize_database(pool: &PgPool) -> Result<()> {
    debug!("Initializing job database tables");
    sqlx::raw_sql(&format!(
        r"
            CREATE TABLE IF NOT EXISTS pg_loco_queue (
                id VARCHAR NOT NULL,
                name VARCHAR NOT NULL,
                task_data JSONB NOT NULL,
                status VARCHAR NOT NULL DEFAULT '{}',
                run_at TIMESTAMPTZ NOT NULL,
                interval BIGINT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                tags JSONB
            );
            ",
        JobStatus::Queued
    ))
    .execute(pool)
    .await?;
    Ok(())
}

/// Add a job
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn enqueue(
    pool: &PgPool,
    name: &str,
    data: JobData,
    run_at: DateTime<Utc>,
    interval: Option<Duration>,
    tags: Option<Vec<String>>,
) -> Result<JobId> {
    let data_json = serde_json::to_value(data)?;
    let tags_json = tags
        .as_ref()
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null));

    #[allow(clippy::cast_possible_truncation)]
    let interval_ms: Option<i64> = interval.map(|i| i.as_millis() as i64);

    let id = Ulid::new().to_string();
    debug!(job_id = %id, job_name = %name, run_at = %run_at, tags = ?tags, "Enqueueing job");
    sqlx::query(
        "INSERT INTO pg_loco_queue (id, task_data, name, run_at, interval, tags) VALUES ($1, $2, $3, \
         $4, $5, $6)",
    )
    .bind(id.clone())
    .bind(data_json)
    .bind(name)
    .bind(run_at)
    .bind(interval_ms)
    .bind(tags_json)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn dequeue(client: &PgPool, worker_tags: &[String]) -> Result<Option<Job>> {
    let mut tx = client.begin().await?;

    // Base query
    let mut query = String::from(
        "SELECT id, name, task_data, status, run_at, interval, tags FROM pg_loco_queue WHERE status = $1 AND run_at <= NOW() "
    );

    // Apply tag filtering logic
    if worker_tags.is_empty() {
        // If worker has no tags, only process jobs with no tags
        query.push_str("AND (tags IS NULL) ");
    } else {
        // If worker has tags, we need a more complex condition
        query.push_str("AND (tags IS NOT NULL) ");

        // In PostgreSQL, we need to build a condition for each tag individually
        let mut conditions = Vec::new();

        for (i, _) in worker_tags.iter().enumerate() {
            // Check if the tag exists as a JSON string in the tags array
            // Using ? operator checks if string exists as array element
            conditions.push(format!("(tags)::jsonb ? ${}", i + 2));
        }

        if !conditions.is_empty() {
            query.push_str(" AND (");
            query.push_str(&conditions.join(" OR "));
            query.push(')');
        }
    }

    query.push_str(" ORDER BY run_at LIMIT 1 FOR UPDATE SKIP LOCKED");

    // Create the query
    let mut db_query = sqlx::query(&query).bind(JobStatus::Queued.to_string());

    // Bind tag parameters
    for tag in worker_tags {
        db_query = db_query.bind(tag);
    }

    let row = db_query
        .map(|row: PgRow| to_job(&row).ok())
        .fetch_optional(&mut *tx)
        .await?
        .flatten();

    if let Some(job) = row {
        trace!(job_id = %job.id, job_name = %job.name, job_tags = ?job.tags, "Dequeueing job for processing");
        sqlx::query("UPDATE pg_loco_queue SET status = $1, updated_at = NOW() WHERE id = $2")
            .bind(JobStatus::Processing.to_string())
            .bind(&job.id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(Some(job))
    } else {
        Ok(None)
    }
}

async fn complete_job(pool: &PgPool, id: &JobId, interval_ms: Option<i64>) -> Result<()> {
    let (status, run_at) = interval_ms.map_or_else(
        || (JobStatus::Completed.to_string(), Utc::now()),
        |interval_ms| {
            (
                JobStatus::Queued.to_string(),
                Utc::now() + chrono::Duration::milliseconds(interval_ms),
            )
        },
    );

    trace!(
        job_id = %id,
        status = %status,
        run_at = %run_at,
        "Marking job as completed"
    );

    sqlx::query(
        "UPDATE pg_loco_queue SET status = $1, updated_at = NOW(), run_at = $2 WHERE id = $3",
    )
    .bind(status)
    .bind(run_at)
    .bind(id)
    .execute(pool)
    .await?;

    Ok(())
}

async fn fail_job(pool: &PgPool, id: &JobId, error: &crate::Error) -> Result<()> {
    let msg = error.to_string();
    debug!(job_id = %id, error = %msg, "Marking job as failed");
    let error_json = serde_json::json!({ "error": msg });
    sqlx::query(
        "UPDATE pg_loco_queue SET status = $1, updated_at = NOW(), task_data = task_data || \
         $2::jsonb WHERE id = $3",
    )
    .bind(JobStatus::Failed.to_string())
    .bind(error_json)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Cancels jobs in the `pg_loco_queue` table by their name.
///
/// This function updates the status of all jobs with the given `name` and a status of
/// [`JobStatus::Queued`] to [`JobStatus::Cancelled`]. The update also sets the `updated_at` timestamp to the
/// current time.
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn cancel_jobs_by_name(pool: &PgPool, name: &str) -> Result<()> {
    debug!(job_name = %name, "Cancelling queued jobs by name");
    sqlx::query(
        "UPDATE pg_loco_queue SET status = $1, updated_at = NOW() WHERE name = $2 AND status = $3",
    )
    .bind(JobStatus::Cancelled.to_string())
    .bind(name)
    .bind(JobStatus::Queued.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Clear all jobs
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn clear(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM pg_loco_queue")
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes jobs from the `pg_loco_queue` table based on their status.
///
/// This function removes all jobs with a status that matches any of the statuses provided
/// in the `status` argument. The statuses are checked against the `status` column in the
/// database, and any matching rows are deleted.
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn clear_by_status(pool: &PgPool, status: Vec<JobStatus>) -> Result<()> {
    let status_in = status
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<String>>();

    debug!(status = ?status, "Clearing jobs by status");
    sqlx::query("DELETE FROM pg_loco_queue WHERE status = ANY($1)")
        .bind(status_in)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes jobs from the `pg_loco_queue` table that are older than a specified number of days.
///
/// This function removes jobs that have a `created_at` timestamp older than the provided
/// number of days. Additionally, if a `status` is provided, only jobs with a status matching
/// one of the provided values will be deleted.
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn clear_jobs_older_than(
    pool: &PgPool,
    age_days: i64,
    status: Option<&Vec<JobStatus>>,
) -> Result<()> {
    let mut query_builder = sqlx::query_builder::QueryBuilder::<sqlx::Postgres>::new(
        "DELETE FROM pg_loco_queue WHERE created_at < NOW() - INTERVAL '1 day' * ",
    );

    query_builder.push_bind(age_days);

    if let Some(status_list) = status {
        if !status_list.is_empty() {
            let status_in = status_list
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<String>>()
                .join(",");

            query_builder.push(format!(" AND status IN ({status_in})"));
        }
    }

    debug!(age_days = age_days, status = ?status, "Clearing older jobs");
    query_builder.build().execute(pool).await?;

    Ok(())
}

/// Requeues jobs from [`JobStatus::Processing`] to [`JobStatus::Queued`].
///
/// This function updates the status of all jobs that are currently in the [`JobStatus::Processing`] state
/// to the [`JobStatus::Queued`] state, provided they have been updated more than the specified age (`age_minutes`).
/// The jobs that meet the criteria will have their `updated_at` timestamp set to the current time.
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn requeue(pool: &PgPool, age_minutes: &i64) -> Result<()> {
    let interval = format!("{age_minutes} MINUTE");

    let query = format!(
        "UPDATE pg_loco_queue SET status = $1, updated_at = NOW() WHERE status = $2 AND updated_at <= NOW() - INTERVAL '{interval}'"
    );

    debug!(age_minutes = age_minutes, "Requeueing stalled jobs");
    sqlx::query(&query)
        .bind(JobStatus::Queued.to_string())
        .bind(JobStatus::Processing.to_string())
        .execute(pool)
        .await?;

    Ok(())
}

/// Ping system
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn ping(pool: &PgPool) -> Result<()> {
    trace!("Pinging job queue database");
    sqlx::query("SELECT id from pg_loco_queue LIMIT 1")
        .execute(pool)
        .await?;
    Ok(())
}

/// Retrieves a list of jobs from the `pg_loco_queue` table in the database.
///
/// This function queries the database for jobs, optionally filtering by their
/// `status`. If a status is provided, only jobs with statuses included in the
/// provided list will be fetched. If no status is provided, all jobs will be
/// returned.
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn get_jobs(
    pool: &PgPool,
    status: Option<&Vec<JobStatus>>,
    age_days: Option<i64>,
) -> Result<Vec<Job>, sqlx::Error> {
    let mut query = String::from("SELECT * FROM pg_loco_queue where true");

    if let Some(status) = status {
        let status_in = status
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<String>>()
            .join(",");
        let _ = write!(query, " AND status in ({status_in})");
    }

    if let Some(age_days) = age_days {
        let _ = write!(
            query,
            " AND created_at <= NOW() - INTERVAL '1 day' * {age_days}"
        );
    }

    debug!(status = ?status, age_days = ?age_days, "Retrieving jobs");
    let rows = sqlx::query(&query).fetch_all(pool).await?;
    let jobs = rows.iter().filter_map(|row| to_job(row).ok()).collect();
    debug!(job_count = rows.len(), "Retrieved jobs from database");
    Ok(jobs)
}

/// Converts a row from the database into a [`Job`] object.
///
/// This function takes a row from the `Postgres` database and manually extracts the necessary
/// fields to populate a [`Job`] object.
///
/// **Note:** This function manually extracts values from the database row instead of using
/// the `FromRow` trait, which would require enabling the 'macros' feature in the dependencies.
/// The decision to avoid `FromRow` is made to keep the build smaller and faster, as the 'macros'
/// feature is unnecessary in the current dependency tree.
fn to_job(row: &PgRow) -> Result<Job> {
    let tags_json: Option<serde_json::Value> = row.try_get("tags").unwrap_or_default();
    let tags = tags_json.and_then(|json_val| {
        if json_val.is_array() {
            let tags_vec: Vec<String> =
                serde_json::from_value(json_val).unwrap_or_else(|_| Vec::new());
            if tags_vec.is_empty() {
                None
            } else {
                Some(tags_vec)
            }
        } else {
            None
        }
    });

    Ok(Job {
        id: row.get("id"),
        name: row.get("name"),
        data: row.get("task_data"),
        status: row.get::<String, _>("status").parse().map_err(|err| {
            let status: String = row.get("status");
            tracing::error!(status, err = %err, "Unsupported job status in database");
            Error::string("invalid job status")
        })?,
        run_at: row.get("run_at"),
        interval: row.get("interval"),
        created_at: row.try_get("created_at").unwrap_or_default(),
        updated_at: row.try_get("updated_at").unwrap_or_default(),
        tags,
    })
}

#[derive(Debug)]
pub struct RunOpts {
    pub num_workers: u32,
    pub poll_interval_sec: u32,
}

/// Create this provider
///
/// # Errors
///
/// This function will return an error if it fails
pub async fn create_provider(qcfg: &PostgresQueueConfig) -> Result<Queue> {
    debug!(
        num_workers = qcfg.num_workers,
        poll_interval = qcfg.poll_interval_sec,
        "Creating job queue provider"
    );
    let pool = connect(qcfg).await.map_err(Box::from)?;
    let registry = JobRegistry::new();
    let token = CancellationToken::new(); // Create the token
    Ok(Queue::Postgres(
        pool,
        Arc::new(tokio::sync::Mutex::new(registry)),
        RunOpts {
            num_workers: qcfg.num_workers,
            poll_interval_sec: qcfg.poll_interval_sec,
        },
        token, // Pass the token
    ))
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveTime, TimeZone};
    use insta::{assert_debug_snapshot, with_settings};
    use sqlx::{query_as, FromRow};
    use tokio::time::sleep;

    use super::*;
    use crate::tests_cfg::{self, postgres::setup_postgres_container};

    fn reduction() -> &'static [(&'static str, &'static str)] {
        &[
            ("[A-Z0-9]{26}", "<REDACTED>"),
            (
                r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z",
                "<REDACTED>",
            ),
        ]
    }

    #[derive(Debug, Serialize, FromRow)]
    pub struct TableInfo {
        pub table_schema: Option<String>,
        pub column_name: Option<String>,
        pub column_default: Option<String>,
        pub is_nullable: Option<String>,
        pub data_type: Option<String>,
        pub is_updatable: Option<String>,
    }

    async fn get_all_jobs(pool: &PgPool) -> Vec<Job> {
        sqlx::query("select * from pg_loco_queue")
            .fetch_all(pool)
            .await
            .expect("get jobs")
            .iter()
            .filter_map(|row| to_job(row).ok())
            .collect()
    }

    async fn get_job(pool: &PgPool, id: &str) -> Job {
        sqlx::query(&format!("select * from pg_loco_queue where id = '{id}'"))
            .fetch_all(pool)
            .await
            .expect("get jobs")
            .first()
            .and_then(|row| to_job(row).ok())
            .expect("job not found")
    }

    // New setup function that uses our testcontainer
    async fn setup_pg_test() -> (
        PgPool,
        testcontainers::ContainerAsync<testcontainers::GenericImage>,
    ) {
        let (pg_url, container) = setup_postgres_container().await;
        let pool = PgPool::connect(&pg_url)
            .await
            .expect("Failed to connect to PostgreSQL");

        // Initialize the database
        initialize_database(&pool)
            .await
            .expect("Failed to initialize database");

        (pool, container)
    }

    #[tokio::test]
    async fn can_initialize_database() {
        let (pool, _container) = setup_pg_test().await;

        let table_info: Vec<TableInfo> = query_as::<_, TableInfo>(
            "SELECT * FROM information_schema.columns WHERE table_name =
    'pg_loco_queue'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_debug_snapshot!(table_info);
    }

    #[tokio::test]
    async fn can_enqueue() {
        let (pool, _container) = setup_pg_test().await;

        let jobs = get_all_jobs(&pool).await;

        assert_eq!(jobs.len(), 0);

        let run_at = Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2023, 1, 15)
                .unwrap()
                .and_time(NaiveTime::from_hms_opt(12, 30, 0).unwrap()),
        );

        let job_data: JobData = serde_json::json!({"user_id": 1});
        assert!(enqueue(
            &pool,
            "PasswordChangeNotification",
            job_data,
            run_at,
            None,
            None
        )
        .await
        .is_ok());

        let jobs = get_all_jobs(&pool).await;

        assert_eq!(jobs.len(), 1);
        with_settings!({
                filters => reduction().iter().map(|&(pattern, replacement)|
        (pattern, replacement)),     }, {
                assert_debug_snapshot!(jobs);
            });
    }

    #[tokio::test]
    async fn can_dequeue() {
        let (pool, _container) = setup_pg_test().await;

        let run_at = Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2023, 1, 15)
                .unwrap()
                .and_time(NaiveTime::from_hms_opt(12, 30, 0).unwrap()),
        );

        let job_data: JobData = serde_json::json!({"user_id": 1});
        assert!(enqueue(
            &pool,
            "PasswordChangeNotification",
            job_data,
            run_at,
            None,
            None
        )
        .await
        .is_ok());

        let job_before_dequeue = get_all_jobs(&pool)
            .await
            .first()
            .cloned()
            .expect("gets first job");

        assert_eq!(job_before_dequeue.status, JobStatus::Queued);

        std::thread::sleep(std::time::Duration::from_secs(1));

        assert!(dequeue(&pool, &[]).await.is_ok());

        let job_after_dequeue = get_all_jobs(&pool)
            .await
            .first()
            .cloned()
            .expect("gets first job");

        assert_ne!(job_after_dequeue.updated_at, job_before_dequeue.updated_at);
        with_settings!({
                filters => reduction().iter().map(|&(pattern, replacement)|
        (pattern, replacement)),     }, {
                assert_debug_snapshot!(job_after_dequeue);
            });
    }

    #[tokio::test]
    async fn can_complete_job_without_interval() {
        let (pool, _container) = setup_pg_test().await;
        tests_cfg::queue::postgres_seed_data(&pool).await;

        let job = get_job(&pool, "01JDM0X8EVAM823JZBGKYNBA99").await;

        assert_eq!(job.status, JobStatus::Queued);
        assert!(complete_job(&pool, &job.id, None).await.is_ok());

        let job = get_job(&pool, "01JDM0X8EVAM823JZBGKYNBA99").await;

        assert_eq!(job.status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn can_complete_job_with_interval() {
        let (pool, _container) = setup_pg_test().await;
        tests_cfg::queue::postgres_seed_data(&pool).await;

        let before_complete_job = get_job(&pool, "01JDM0X8EVAM823JZBGKYNBA98").await;

        assert_eq!(before_complete_job.status, JobStatus::Completed);

        std::thread::sleep(std::time::Duration::from_secs(1));

        assert!(complete_job(&pool, &before_complete_job.id, Some(10))
            .await
            .is_ok());

        let after_complete_job = get_job(&pool, "01JDM0X8EVAM823JZBGKYNBA98").await;

        assert_ne!(
            after_complete_job.updated_at,
            before_complete_job.updated_at
        );
        with_settings!({
                filters => reduction().iter().map(|&(pattern, replacement)| (pattern,
        replacement)),     }, {
                assert_debug_snapshot!(after_complete_job);
            });
    }

    #[tokio::test]
    async fn can_fail_job() {
        let (pool, _container) = setup_pg_test().await;
        tests_cfg::queue::postgres_seed_data(&pool).await;

        let before_fail_job = get_job(&pool, "01JDM0X8EVAM823JZBGKYNBA97").await;

        std::thread::sleep(std::time::Duration::from_secs(1));

        assert!(fail_job(
            &pool,
            &before_fail_job.id,
            &crate::Error::string("some error")
        )
        .await
        .is_ok());

        let after_fail_job = get_job(&pool, "01JDM0X8EVAM823JZBGKYNBA97").await;

        assert_ne!(after_fail_job.updated_at, before_fail_job.updated_at);
        with_settings!({
                filters => reduction().iter().map(|&(pattern, replacement)| (pattern,
        replacement)),     }, {
                assert_debug_snapshot!(after_fail_job);
            });
    }

    #[tokio::test]
    async fn can_cancel_job_by_name() {
        let (pool, _container) = setup_pg_test().await;
        tests_cfg::queue::postgres_seed_data(&pool).await;

        let count_cancelled_jobs = get_all_jobs(&pool)
            .await
            .iter()
            .filter(|j| j.status == JobStatus::Cancelled)
            .count();

        assert_eq!(count_cancelled_jobs, 1);

        assert!(cancel_jobs_by_name(&pool, "UserAccountActivation")
            .await
            .is_ok());

        let count_cancelled_jobs = get_all_jobs(&pool)
            .await
            .iter()
            .filter(|j| j.status == JobStatus::Cancelled)
            .count();

        assert_eq!(count_cancelled_jobs, 2);
    }

    #[tokio::test]
    async fn can_clear() {
        let (pool, _container) = setup_pg_test().await;
        tests_cfg::queue::postgres_seed_data(&pool).await;

        let job_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_loco_queue")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_ne!(job_count, 0);

        assert!(clear(&pool).await.is_ok());
        let job_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_loco_queue")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(job_count, 0);
    }

    #[tokio::test]
    async fn can_clear_by_status() {
        let (pool, _container) = setup_pg_test().await;
        tests_cfg::queue::postgres_seed_data(&pool).await;

        let jobs = get_all_jobs(&pool).await;

        assert_eq!(jobs.len(), 14);
        assert_eq!(
            jobs.iter()
                .filter(|j| j.status == JobStatus::Completed)
                .count(),
            3
        );
        assert_eq!(
            jobs.iter()
                .filter(|j| j.status == JobStatus::Failed)
                .count(),
            2
        );

        assert!(
            clear_by_status(&pool, vec![JobStatus::Completed, JobStatus::Failed])
                .await
                .is_ok()
        );
        let jobs = get_all_jobs(&pool).await;

        assert_eq!(jobs.len(), 9);
        assert_eq!(
            jobs.iter()
                .filter(|j| j.status == JobStatus::Completed)
                .count(),
            0
        );
        assert_eq!(
            jobs.iter()
                .filter(|j| j.status == JobStatus::Failed)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn can_clear_jobs_older_than() {
        let (pool, _container) = setup_pg_test().await;

        sqlx::query(
           r"INSERT INTO pg_loco_queue (id, name, task_data, status, run_at,created_at, updated_at) VALUES
             ('job1', 'Test Job 1', '{}', 'queued', NOW(), NOW() - INTERVAL '15days', NOW()),
             ('job2', 'Test Job 2', '{}', 'queued', NOW(),NOW() - INTERVAL '5 days', NOW()),
             ('job3', 'Test Job 3', '{}','queued', NOW(), NOW(), NOW())"
            )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(get_all_jobs(&pool).await.len(), 3);
        assert!(clear_jobs_older_than(&pool, 10, None).await.is_ok());
        assert_eq!(get_all_jobs(&pool).await.len(), 2);
    }

    #[tokio::test]
    async fn can_clear_jobs_older_than_with_status() {
        let (pool, _container) = setup_pg_test().await;

        sqlx::query(
           r"INSERT INTO pg_loco_queue (id, name, task_data, status, run_at,created_at, updated_at) VALUES
             ('job1', 'Test Job 1', '{}', 'completed', NOW(), NOW() - INTERVAL '20days', NOW()),
             ('job2', 'Test Job 2', '{}', 'failed', NOW(),NOW() - INTERVAL '15 days', NOW()),
             ('job3', 'Test Job 3', '{}', 'completed', NOW(),NOW() - INTERVAL '5 days', NOW()),
             ('job4', 'Test Job 3', '{}','cancelled', NOW(), NOW(), NOW())"
            )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(get_all_jobs(&pool).await.len(), 4);
        assert!(clear_jobs_older_than(
            &pool,
            10,
            Some(&vec![JobStatus::Cancelled, JobStatus::Completed])
        )
        .await
        .is_ok());

        assert_eq!(get_all_jobs(&pool).await.len(), 3);
    }

    #[tokio::test]
    async fn can_get_jobs() {
        let (pool, _container) = setup_pg_test().await;
        tests_cfg::queue::postgres_seed_data(&pool).await;

        assert_eq!(
            get_jobs(&pool, Some(&vec![JobStatus::Failed]), None)
                .await
                .expect("get jobs")
                .len(),
            2
        );
        assert_eq!(
            get_jobs(
                &pool,
                Some(&vec![JobStatus::Failed, JobStatus::Completed]),
                None
            )
            .await
            .expect("get jobs")
            .len(),
            5
        );
        assert_eq!(
            get_jobs(&pool, None, None).await.expect("get jobs").len(),
            14
        );
    }

    #[tokio::test]
    async fn can_get_jobs_with_age() {
        let (pool, _container) = setup_pg_test().await;

        sqlx::query(
            r"INSERT INTO pg_loco_queue (id, name, task_data, status, run_at,created_at, updated_at) VALUES
             ('job1', 'Test Job 1', '{}', 'completed', NOW(), NOW() - INTERVAL '20days', NOW()),
             ('job2', 'Test Job 2', '{}', 'failed', NOW(),NOW() - INTERVAL '15 days', NOW()),
             ('job3', 'Test Job 3', '{}', 'completed', NOW(),NOW() - INTERVAL '5 days', NOW()),
             ('job4', 'Test Job 3', '{}','cancelled', NOW(), NOW(), NOW())"
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            get_jobs(
                &pool,
                Some(&vec![JobStatus::Failed, JobStatus::Completed]),
                Some(11)
            )
            .await
            .expect("get jobs")
            .len(),
            2
        );
    }

    #[tokio::test]
    async fn can_requeue() {
        let (pool, _container) = setup_pg_test().await;

        sqlx::query(
            r"INSERT INTO pg_loco_queue (id, name, task_data, status, run_at,created_at, updated_at) VALUES
             ('job1', 'Test Job 1', '{}', 'processing', NOW(),NOW(), NOW() - INTERVAL '20 minutes'),
             ('job2', 'Test Job 2', '{}', 'processing', NOW(),NOW(), NOW() - INTERVAL '5 minutes'),
             ('job3', 'Test Job 3', '{}', 'completed', NOW(),NOW(),NOW() - INTERVAL '5 minutes'),
             ('job4', 'Test Job 4', '{}', 'queued', NOW(),NOW(), NOW()),
             ('job4', 'Test Job 5', '{}', 'processing', NOW(), NOW(), NOW())"
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            get_jobs(&pool, Some(&vec![JobStatus::Processing]), None)
                .await
                .expect("get jobs")
                .len(),
            3
        );
        assert_eq!(
            get_jobs(&pool, Some(&vec![JobStatus::Queued]), None)
                .await
                .expect("get jobs")
                .len(),
            1
        );

        requeue(&pool, &10).await.expect("update jobs");

        assert_eq!(
            get_jobs(&pool, Some(&vec![JobStatus::Processing]), None)
                .await
                .expect("get jobs")
                .len(),
            2
        );
        assert_eq!(
            get_jobs(&pool, Some(&vec![JobStatus::Queued]), None)
                .await
                .expect("get jobs")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn can_handle_worker_panic() {
        let (pool, _container) = setup_pg_test().await;

        let job_data: JobData = serde_json::json!(null);
        let job_id = enqueue(&pool, "PanicJob", job_data, Utc::now(), None, None)
            .await
            .expect("Failed to enqueue job");

        struct PanicWorker;
        #[async_trait::async_trait]
        impl BackgroundWorker<()> for PanicWorker {
            fn build(_ctx: &crate::app::AppContext) -> Self {
                Self
            }
            async fn perform(&self, _args: ()) -> crate::Result<()> {
                panic!("intentional panic for testing");
            }
        }

        let mut registry = JobRegistry::new();
        assert!(registry
            .register_worker("PanicJob".to_string(), PanicWorker)
            .is_ok());

        // Get the initial job state
        let job = get_job(&pool, &job_id).await;
        assert_eq!(job.status, JobStatus::Queued);

        // Start the worker
        let opts = RunOpts {
            num_workers: 1,
            poll_interval_sec: 1,
        };
        let token = CancellationToken::new();
        let handles = registry.run(&pool, &opts, &token, &[]);

        // Wait a bit for the worker to process the job
        sleep(Duration::from_secs(1)).await;

        // Stop the worker
        for handle in handles {
            handle.abort();
        }

        // Verify the job is marked as failed
        let failed_job = get_job(&pool, &job_id).await;
        assert_eq!(failed_job.status, JobStatus::Failed);

        // Verify the error message stored in job data
        let error_msg = failed_job
            .data
            .as_array()
            .and_then(|arr| arr.get(1))
            .and_then(|obj| obj.as_object())
            .and_then(|obj| obj.get("error"))
            .and_then(|v| v.as_str())
            .expect("Expected error message in job data");
        assert!(
            error_msg.contains("intentional panic for testing"),
            "Error message '{error_msg}' did not contain expected text"
        );
    }

    #[tokio::test]
    async fn can_dequeue_with_tags() {
        let (pool, _container) = setup_pg_test().await;

        // Add a job with email tag
        let run_at = Utc::now() - chrono::Duration::minutes(5); // In the past so it's ready to process
        let job_data = serde_json::json!({"user_id": 1});

        // Insert email job
        let email_tags = Some(vec!["email".to_string()]);
        let email_id = enqueue(
            &pool,
            "EmailNotification",
            job_data.clone(),
            run_at,
            None,
            email_tags,
        )
        .await
        .expect("Failed to enqueue email job");

        // Insert job with "sms" tag
        let sms_tags = Some(vec!["sms".to_string()]);
        let sms_id = enqueue(
            &pool,
            "SmsNotification",
            job_data.clone(),
            run_at,
            None,
            sms_tags,
        )
        .await
        .expect("Failed to enqueue sms job");

        // Insert job with multiple tags
        let multi_tags = Some(vec!["email".to_string(), "priority".to_string()]);
        let multi_id = enqueue(
            &pool,
            "PriorityEmail",
            job_data.clone(),
            run_at,
            None,
            multi_tags,
        )
        .await
        .expect("Failed to enqueue multi-tag job");

        // Insert job with no tags
        let no_tag_id = enqueue(
            &pool,
            "GenericNotification",
            job_data.clone(),
            run_at,
            None,
            None,
        )
        .await
        .expect("Failed to enqueue untagged job");

        // Verify all jobs are in the database
        let all_jobs = get_all_jobs(&pool).await;
        assert_eq!(all_jobs.len(), 4);

        // 1. Worker with no tags should only get untagged jobs
        let job = dequeue(&pool, &[]).await.expect("dequeue failed");
        assert!(job.is_some());
        let job = job.unwrap();
        assert_eq!(job.id, no_tag_id);
        assert!(job.tags.is_none());

        // Mark the job as completed to remove it from the queued items
        complete_job(&pool, &job.id, None)
            .await
            .expect("Failed to complete job");

        // 2. Worker with "email" tag should get one of the email-tagged jobs
        let job = dequeue(&pool, &["email".to_string()])
            .await
            .expect("dequeue failed");
        assert!(job.is_some());
        let job = job.unwrap();
        assert!(
            job.id == email_id || job.id == multi_id,
            "Expected either email job or multi-tag job"
        );
        assert!(job.tags.is_some());

        // Mark the job as completed
        complete_job(&pool, &job.id, None)
            .await
            .expect("Failed to complete job");

        // 3. Worker with "email" tag should get the remaining email job
        let job = dequeue(&pool, &["email".to_string()])
            .await
            .expect("dequeue failed");
        assert!(job.is_some());
        let job = job.unwrap();
        assert!(
            job.id == email_id || job.id == multi_id,
            "Expected either email job or multi-tag job"
        );
        assert!(job.tags.is_some());

        // Mark the job as completed
        complete_job(&pool, &job.id, None)
            .await
            .expect("Failed to complete job");

        // 4. Worker with "sms" tag should get the sms job
        let job = dequeue(&pool, &["sms".to_string()])
            .await
            .expect("dequeue failed");
        assert!(job.is_some());
        let job = job.unwrap();
        assert_eq!(job.id, sms_id);
        assert!(job.tags.is_some());

        // Mark the job as completed
        complete_job(&pool, &job.id, None)
            .await
            .expect("Failed to complete job");

        // 5. No more jobs should be available
        let job = dequeue(&pool, &["email".to_string()])
            .await
            .expect("dequeue failed");
        assert!(job.is_none());

        // 6. No more jobs should be available for untagged worker
        let job = dequeue(&pool, &[]).await.expect("dequeue failed");
        assert!(job.is_none());
    }
}
