use csv::WriterBuilder;
use log::{debug, info, trace};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::OpenOptions;
use std::io::BufReader;
use std::io::BufWriter;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Job {
    id: u32,
    status: String,
    stage: String,
    name: String,
    #[serde(rename = "ref")]
    ref_str: String,
    duration: Option<f32>,
    queued_duration: Option<f32>,
    created_at: String,
    failure_reason: Option<String>,
    web_url: String,
}

const BASE_URL: &str = "https://gitlab.parity.io/api/v4/projects/674/jobs";

fn main() -> Result<(), anyhow::Error> {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );
    let jobs_csv =
        env::var("ZOMBIE_JOBS_CSV_FILE").unwrap_or_else(|_| String::from("zombie_jobs.csv"));
    let base_url = env::var("ZOMBIE_JOBS_BASE_URL").unwrap_or_else(|_| String::from(BASE_URL));
    let token = env::var("ZOMBIE_JOBS_TOKEN").ok();

    // open store
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&jobs_csv)?;
    let br = BufReader::new(file);
    let mut rdr = csv::Reader::from_reader(br);
    let mut stored_jobs: Vec<Job> = rdr.deserialize().map(|r| r.unwrap()).collect();
    let last_job_id = if let Some(job) = stored_jobs.first() {
        job.id
    } else {
        0
    };

    // fetch page/s
    let mut next_page = Some(1);
    let mut jobs: Vec<Job> = vec![];
    debug!("last stored id: {last_job_id}");
    while next_page.is_some() {
        let page_num = next_page.unwrap(); // SAFETY: we check if some before.
        info!("fetching page {page_num}");
        let page = fetch_page(
            &base_url,
            token.clone(),
            &vec!["failed", "success"],
            page_num,
        )?;
        let mut reach_last_job_stored = false;
        let mut zombienet_jobs: Vec<Job> = page
            .jobs
            .into_iter()
            .filter(|job| {
                if job.stage == "zombienet" {
                    if job.id > last_job_id {
                        true
                    } else {
                        trace!("reached at job id: {}", job.id);
                        // set that we don't need to fetch anymore
                        reach_last_job_stored = true;
                        false
                    }
                } else {
                    false
                }
            })
            .collect();
        jobs.append(&mut zombienet_jobs);
        next_page = if page_num > 10 || reach_last_job_stored {
            None
        } else {
            page.next_page
        };
        debug!(
            "fetching next page {:?},  accumulator len: {}",
            next_page,
            jobs.len()
        );
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&jobs_csv)?;

    let br = BufWriter::new(file);
    let mut wtr = WriterBuilder::new().from_writer(br);

    jobs.append(&mut stored_jobs);
    for job in jobs {
        wtr.serialize(job)?;
    }
    wtr.flush()?;

    Ok(())
}

struct Page {
    jobs: Vec<Job>,
    next_page: Option<u32>,
}

fn fetch_page<T: AsRef<str>>(
    base_url: &str,
    token: Option<String>,
    scopes: &[T],
    page: u32,
) -> Result<Page, anyhow::Error> {
    let mut parts: Vec<String> = scopes
        .iter()
        .map(|scope| format!("scope[]={}", scope.as_ref()))
        .collect();
    parts.extend_from_slice(&vec![format!("page={page}"), String::from("per_page=100")]);

    let url = format!("{}?{}", base_url, parts.join("&"));
    debug!("url: {url}");
    let client = reqwest::blocking::Client::new();
    let req = client.get(url).header("Accept", "application/json");
    let req = if let Some(token) = token {
        req.header("PRIVATE-TOKEN", token)
    } else {
        req
    };

    let res = req.send()?;

    let headers = res.headers();
    let next_page = if let Some(next_page) = headers.get("x-next-page") {
        Some(next_page.to_str()?.parse()?)
    } else {
        None
    };

    let text = res.text()?;

    let page = Page {
        jobs: serde_json::from_str(&text)?,
        next_page,
    };

    Ok(page)
}
