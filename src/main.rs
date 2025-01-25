#![allow(clippy::needless_return)]

use csv::WriterBuilder;
use log::warn;
use log::{debug, info, trace};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::OpenOptions;
use std::io::BufReader;
use std::io::BufWriter;
use futures_util::future::try_join_all;
use regex::Regex;
use std::path::PathBuf;
use sqlite::Value::{String as sqlString, Integer, Null, Float};



use clap::{Parser, ValueEnum};

#[derive(Parser, Debug)]
struct Args {
    #[clap(value_enum, default_value_t=StoreProvider::Db)]
    store: StoreProvider,
}

#[derive(ValueEnum, Clone, Debug)]
enum StoreProvider {
    Csv,
    Db,
}

#[derive(Debug, Serialize,Deserialize, Clone)]
enum Fixes {
    #[allow(clippy::upper_case_acronyms)]
    #[serde(rename = "AlreadyFixedNeedsRebase_PJS")]
    PJS,
    #[serde(rename = "AlreadyFixedNeedsRebase_Deleting")]
    Deleting
}


#[derive(Debug, Serialize, Deserialize,Clone, Default)]
enum Cause {
    #[default]
    Unchecked,
    // Deploy error by some kubectl cmd error
    ZombienetDeployment,
    FileServerDNSIssue,
    Infra,
    InfraKilled,
    Assertion,
    AlreadyMerged,
    AlreadyFixedNeedsRebase(Fixes)
}


impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let printable = match self {
            Cause::Unchecked => "Unchecked",
            Cause::ZombienetDeployment => "ZombienetDeployment",
            Cause::FileServerDNSIssue => "FileServerDNSIssue",
            Cause::Infra => "Infra",
            Cause::InfraKilled => "InfraKilled",
            Cause::Assertion => "Assertion",
            Cause::AlreadyMerged => "AlreadyMerged",
            Cause::AlreadyFixedNeedsRebase(fixes) => {
                match fixes {
                    Fixes::PJS => "AlreadyFixedNeedsRebase_PJS",
                    Fixes::Deleting => "AlreadyFixedNeedsRebase_Deleting",
                }
            }
        };
        write!(f, "{:?}", printable)
    }
}

const SCHEMA_DB: &str = "CREATE TABLE IF NOT EXISTS jobs ( \
    id INTEGER PRIMARY KEY, \
    status TEXT NOT NULL, \
    stage TEXT NOT NULL, \
    name TEXT NOT NULL, \
    ref_str TEXT NOT NULL, \
    duration REAL, \
    queue_duration REAL, \
    created_at TEXT NOT NULL, \
    finished_at TEXT, \
    failure_reason TEXT, \
    web_url TEXT NOT NULL, \
    checked_cause TEXT, \
    zombie_version TEXT
    );
";



#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Job {
    id: u32,
    status: String,
    stage: String,
    name: String,
    #[serde(rename = "ref")]
    ref_str: String,
    duration: Option<f32>,
    queued_duration: Option<f32>,
    created_at: String,
    finished_at: Option<String>,
    failure_reason: Option<String>,
    web_url: String,
    #[serde(default)]
    checked_cause: Cause,
    // only added to failed jobs
    zombie_version: Option<String>
}

const BASE_URL: &str = "https://gitlab.parity.io/api/v4/projects/674/jobs";
const MAX_PAGES: u32 = 1000; // 1000 pages max

pub trait Store {
    // fn open(file: &str) -> Result<(), anyhow::Error>;
    fn last_id(&self) -> u32;
    fn append(&mut self, job: Vec<Job>) -> Result<(), anyhow::Error>;
    fn save(&self) -> Result<(), anyhow::Error>;
}
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug)]
struct CSV {
    jobs: Vec<Job>,
    stored_jobs: Vec<Job>,
    file: PathBuf
}

impl CSV {
    fn new(file: &str) -> Result<Self, anyhow::Error> {
    let file_path: PathBuf = file.into();
    // open store
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&file_path)?;

        let br = BufReader::new(file);
        let mut rdr = csv::Reader::from_reader(br);
        let stored_jobs: Vec<Job> = rdr.deserialize().map(|r| r.unwrap()).collect();

        Ok(CSV {
            file: file_path,
            jobs: Default::default(),
            stored_jobs,
        })
    }
}

impl Store for CSV {
    fn last_id(&self) -> u32 {
        if let Some(job) = self.stored_jobs.first() {
            job.id
        } else {
            0
        }
    }

    fn append(&mut self,  mut jobs: Vec<Job>) -> Result<(), anyhow::Error> {
        self.jobs.append(&mut jobs);
        Ok(())
    }

    fn save(&self) -> Result<(), anyhow::Error> {
        let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&self.file)?;

        let br = BufWriter::new(file);
        let mut wtr = WriterBuilder::new().from_writer(br);

        for job in &self.jobs {
            wtr.serialize(job)?;
        }

        for job in &self.stored_jobs {
            wtr.serialize(job)?;
        }

        wtr.flush()?;
        Ok(())
    }
}


pub struct Db {
    #[allow(dead_code)]
    file: PathBuf,
    // Store the connection
    conn: sqlite::Connection,
}

impl Db {
    fn new(file: &str) -> Result<Self, anyhow::Error> {
        let file_path: PathBuf = file.into();
        // open store
        let err_msg = format!("can't open file {:?}", file_path);
        let connection = sqlite::open(&file_path).expect(&err_msg);
        // create scheme IFF not exist
        connection.execute(SCHEMA_DB)?;
        Ok(Self { file: file_path, conn: connection })
    }
}

impl Store for Db {
    fn last_id(&self) -> u32 {
        let mut statement = self.conn.prepare("SELECT MAX(id) from jobs").expect("Select last id statement should be ok. qed");
        if let Ok(sqlite::State::Row)  = statement.next() {
            let id = statement.read::<i64, _>(0).unwrap_or(0);
            id.try_into().unwrap_or(0)
        } else {
            0
        }
    }

    fn append(&mut self,  jobs: Vec<Job>) -> Result<(), anyhow::Error> {
        let query = "INSERT into jobs VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)";

        for job in jobs {
            let duration = if let Some(d) = job.duration {
                Float(d as f64)
            } else {
                Null
            };

            let queued_duration = if let Some(d) = job.queued_duration {
                Float(d as f64)
            } else {
                Null
            };

            let finished_at = if let Some(f) = job.finished_at {
                sqlString(f)
            } else {
                Null
            };

            let failure_reason = if let Some(f) = job.failure_reason {
                sqlString(f)
            } else {
                Null
            };

            let zombie_version = if let Some(z) = job.zombie_version {
                sqlString(z)
            } else {
                Null
            };

            trace!("job id: {}", job.id);
            let mut statement = self.conn.prepare(query).unwrap();
            statement.bind( &[
                (1, Integer(job.id as i64)),
                (2, sqlString(job.status)),
                (3, sqlString(job.stage)),
                (4, sqlString(job.name)),
                (5, sqlString(job.ref_str)),
                (6, duration),
                (7, queued_duration),
                (8, sqlString(job.created_at)),
                (9,finished_at),
                (10, failure_reason),
                (11, sqlString(job.web_url)),
                (12, sqlString(job.checked_cause.to_string())),
                (13, zombie_version),
                ][..]
            ).expect("Bind should works. qed");


            if let Err(e) = statement.next() {
                if let Some("UNIQUE constraint failed: jobs.id") = e.message.as_deref() {
                    warn!("WARN: duplicated job id {}, skipping. Err in insert {:?}", job.id, e);
                } else {
                    return Err(e.into());
                }
            };
        }

        Ok(())
    }

    fn save(&self) -> Result<(), anyhow::Error> {
        Ok(())
    }
}

fn get_store(provider: StoreProvider, jobs_file: &str) -> Result<Box<dyn Store>, anyhow::Error> {
    match provider {
        StoreProvider::Csv => Ok(Box::new(CSV::new(jobs_file)?)),
        StoreProvider::Db =>  Ok(Box::new(Db::new(jobs_file)?)),
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );

    let args = Args::parse();
    debug!("Args: {args:?}");

    let jobs_file =
        env::var("ZOMBIE_JOBS_FILE").unwrap_or_else(|_| String::from("zombie_jobs.csv"));

    let mut store = get_store(args.store, &jobs_file)?;

    let base_url = env::var("ZOMBIE_JOBS_BASE_URL").unwrap_or_else(|_| String::from(BASE_URL));
    let max_mages: u32 = env::var("ZOMBIE_JOBS_MAX_PAGES").unwrap_or_else(|_| MAX_PAGES.to_string()).parse().unwrap_or(MAX_PAGES);
    let token = env::var("ZOMBIE_JOBS_TOKEN").ok();
    // from where we look jobs, format should be "%Y-%m-%dTHH:MM:ssZ"
    let base_date = env::var("ZOMBIE_JOBS_BASE_DATE");
    // until where we look jobs, format should be "%Y-%m-%dTHH:MM:ssZ"
    let end_date = env::var("ZOMBIE_JOBS_END_DATE");

    let version_re = Regex::new(r#"docker\.io\/paritytech\/zombienet:(v[0-9].[0-9].[0-9]+)"#).expect("version regex should be valid. qed");


    let last_job_id = store.last_id();

    let client = reqwest::Client::new();
    // fetch page/s
    let mut next_page = Some(1);
    info!("last stored id: {last_job_id}");
    while next_page.is_some() {
        let page_num = next_page.unwrap(); // SAFETY: we check if some before.
        info!("fetching page {page_num}");
        let page = fetch_page(
            &base_url,
            token.clone(),
            &["failed", "success"],
            page_num,
            &client
        ).await?;
        let mut reach_last_job_stored = false;
        let mut zombienet_jobs: Vec<Job> = page
            .jobs
            .into_iter()
            .filter(|job| {
                if job.stage == "zombienet" {
                    trace!("last: {last_job_id}, base {base_date:?}, end {end_date:?}, job.created_at {}", job.created_at);
                    let base_date_ok = if let Ok(base_date) = &base_date {
                        job.created_at > *base_date
                    } else {
                        // we don't have any limit
                        true
                    };

                    let end_date_ok = if let Ok(end_date) = &end_date {
                        job.created_at < *end_date
                    } else {
                        // we don't have any limit
                        true
                    };
                    if job.id > last_job_id && base_date_ok && end_date_ok {
                        true
                    } else {
                        let msg = if job.id <= last_job_id {
                            format!("at job id: {} (last stored id {})", job.id, last_job_id)
                        } else {
                            format!("at date {} (range base: {base_date:?} - end: {end_date:?})", job.created_at)
                        };

                        if !reach_last_job_stored {
                            info!("Reached limit {msg}");
                        }

                        reach_last_job_stored = true;

                        false
                    }
                } else {
                    trace!("not zombie job, cretaed at {}", job.created_at);
                    false
                }
            })
            .collect();

        // analyze in chunks
        for chunk in zombienet_jobs.chunks_mut(10) {
            let mut futures = vec![];
            for job in chunk {
                if job.status == "failed" {
                    futures.push(
                        analyze_job(job, &client, &version_re)
                    );
                }
            }

            let _ = try_join_all(futures).await;

        }

        store.append(zombienet_jobs)?;

        next_page = if page_num >  max_mages || reach_last_job_stored {
            None
        } else {
            page.next_page
        };
        debug!(
            "fetching next page {:?}",
            next_page,
        );
    }

    store.save()?;

    Ok(())
}

struct Page {
    jobs: Vec<Job>,
    next_page: Option<u32>,
}

async fn analyze_job<'a>(job: &'a mut Job, client: &reqwest::Client, re: &Regex) -> Result<(), anyhow::Error> {
    //get raw log
    let url = format!("{}/raw", job.web_url);
    let res = client.get(url).send().await?;
    let log = res.text().await?;

    // Known cases
    const ZOMBIE_START: &str = "Tests are currently running. Results will appear at the end";
    const ZOMBIE_FILESERVER_ISSUE: &str = "unable to resolve host address 'fileserver'";
    const ZOMBIE_DEPLOYED: &str = "Network launched";
    const ZOMBIE_TEST_FAIL: &str = "One or more of your test failed";
    const ZOMBIE_DELETING: &str = "Deleting network";
    const ZOMBIE_KILLED_1: &str = "Killed";
    const ZOMBIE_KILLED_2: &str = "Exit status is 137";
    const ZOMBIE_PJS_UPGRADE: &str = "Error: createType(ExtrinsicUnknown)";
    const ZOMBIE_ALREADY_MERGED: &str = "fatal: couldn't find remote ref";

    // First set the zombie version
    let cap = re.captures(&log);
    if let Some(cap) = cap {
        if let Some(ver) = cap.get(1) {
            trace!("detected version: {}", ver.as_str());
            job.zombie_version = Some(ver.as_str().to_string());
        }
    }

    // check first if the job was `killed`, should show something like
    // /home/nonroot/zombie-net/scripts/ci/run-test-local-env-manager.sh: line 164:    96 Killed                  zombie -c $CONCURRENCY test $i
    // + EXIT_STATUS=137
    // 2024-10-04 07:17:37 - INFO - Exit status is 137
    if log.contains(ZOMBIE_KILLED_1) && log.contains(ZOMBIE_KILLED_2) {
        // killed
        job.checked_cause = Cause::InfraKilled;
        return Ok(())
    }

    // if zombie start
    if log.contains(ZOMBIE_START) {
        if log.contains(ZOMBIE_DEPLOYED) {
            // deploy ok
            if log.contains(ZOMBIE_PJS_UPGRADE) {
                // needs to upgrade to the latest version
                //job.checked_cause = Cause::AlreadyFixedNeedsRebase("Already fixed. PJS version".into());
                job.checked_cause = Cause::AlreadyFixedNeedsRebase(Fixes::PJS);
            } else if log.contains(ZOMBIE_TEST_FAIL) {
                // assertion fails
                job.checked_cause = Cause::Assertion;
            } else if log.contains(ZOMBIE_DELETING) {
                    // error deleting network, should be already fixed
                    // job.checked_cause = Cause::AlreadyFixedNeedsRebase("Already fixed. Deleting".into());
                    job.checked_cause = Cause::AlreadyFixedNeedsRebase(Fixes::Deleting);
            }
        } else {
            // but not get the deployed message
            if log.contains(ZOMBIE_FILESERVER_ISSUE) {
                // Fileserver dns issue
                job.checked_cause = Cause::FileServerDNSIssue;
            } else {
                job.checked_cause = Cause::ZombienetDeployment;
            }
        }
    } else {
        // zombienet didn't start
        if log.contains(ZOMBIE_ALREADY_MERGED) {
            job.checked_cause = Cause::AlreadyMerged
        } else {
            job.checked_cause = Cause::Infra
        }
    }

    Ok(())
}

async fn fetch_page<T: AsRef<str>>(
    base_url: &str,
    token: Option<String>,
    scopes: &[T],
    page: u32,
    client: &reqwest::Client
) -> Result<Page, anyhow::Error> {
    let mut parts: Vec<String> = scopes
        .iter()
        .map(|scope| format!("scope[]={}", scope.as_ref()))
        .collect();
    parts.extend_from_slice(&[format!("page={page}"), String::from("per_page=100")]);

    let url = format!("{}?{}", base_url, parts.join("&"));
    debug!("url: {url}");
    let req = client.get(url).header("Accept", "application/json");
    let req = if let Some(token) = token {
        req.header("PRIVATE-TOKEN", token)
    } else {
        req
    };

    let res = req.send().await?;

    let headers = res.headers();
    let next_page = if let Some(next_page) = headers.get("x-next-page") {
        Some(next_page.to_str()?.parse()?)
    } else {
        None
    };

    let text = res.text().await?;

    let page = Page {
        jobs: serde_json::from_str(&text)?,
        next_page,
    };

    Ok(page)
}
