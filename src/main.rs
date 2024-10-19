#![feature(try_blocks)]

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_JOB_QUEUE: usize = 64;

static REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^.+/videos").unwrap());

use std::{
    collections::HashMap,
    env,
    ffi::CString,
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{Arc, LazyLock},
    thread::sleep,
    time::Duration,
};

use thiserror::Error;

use axum::{
    body::Body,
    extract::{self, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use chrono::{DateTime, Utc};
use rand::random;
use regex::Regex;
use serde_derive::{Deserialize, Serialize};

use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex,
};
use tower_http::services::ServeFile;

pub mod video;
use crate::video::*;

use tracing::{debug, info, trace};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Error, Debug)]
pub enum OptimizationServerError {
    #[error("Failed to write to end file")]
    FailedWrite,
    #[error("Failed to get JOB_ID")]
    GetJob,
}

// Struct that is used to request a job be added to the system
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct JobRecieved {
    pub url: String,
    pub file_extension: String,
    pub device_id: String,
    pub item_id: String,
    pub item: String,
}

// Struct that is built when a job is requested, it takes in [["crate::JobRecieved"]] when building
// and adds timestamp and other needed variables.
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord, Deserialize, Serialize)]
pub struct Job {
    pub id: u32,
    pub status: JobStatus,
    progress: u32,
    output_path: String,
    input_url: String,
    device_id: String,
    timestamp: DateTime<Utc>,
    file_extension: String,
    size: u64,
    item: String,
}

// Struct used to define what the status of a job is
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord, Copy, Deserialize, Serialize)]
pub enum JobStatus {
    Queued,
    Optimizing,
    Completed,
    Failed,
    Cancelled,
}

#[tokio::main]
async fn main() {
    // Enable logging with the RUST_LOG environment variabe
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    let ip: String = env::var("APPLICATION_IP").unwrap_or("0.0.0.0".to_string());

    let port: String = env::var("APPLICATION_PORT").unwrap_or("3000".to_string());

    let (send_channel, recieve_channel): (Sender<u32>, Receiver<u32>) =
        mpsc::channel(MAX_JOB_QUEUE);

    let recieve_channel_safe = Arc::new(Mutex::new(recieve_channel));

    let job_queue_arc: Arc<Mutex<HashMap<u32, Mutex<Job>>>> = Arc::new(Mutex::new(HashMap::new()));
    let cloned = Arc::clone(&job_queue_arc);
    let cloned2 = Arc::clone(&job_queue_arc);
    let cloned3 = Arc::clone(&job_queue_arc);
    let cloned4 = Arc::clone(&job_queue_arc);

    tokio::spawn(async move { handle_jobs(cloned, Arc::clone(&recieve_channel_safe)).await });

    let application_router: Router = Router::new()
        .route("/info", get(info))
        .route(
            "/add-job",
            post(move |body: Json<JobRecieved>| add_job(body, cloned2, send_channel)),
        )
        .route(
            "/get-job",
            get(move |request: Request| get_job(request, cloned3)),
        )
        .route(
            "/get-file",
            get(move |request: Request| get_file(request, cloned4)),
        );

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", ip, port))
        .await
        .expect("TCPListener failed to bind, is it already in use?");

    info!(
        "Started the Optimization Server Version: {}, bound to {}:{}",
        VERSION, ip, port
    );

    axum::serve(listener, application_router)
        .await
        .expect("Server Died, have fun :D");
}

async fn info(request: Request) -> String {
    let version_section = format!("Streamyfin Optimization Server Version: {}", VERSION);

    let uri = request.uri().to_owned();

    let request_section = format!(
        "Request URL: {:?}://{:?}{}",
        uri.scheme_str(),
        uri.host(),
        uri.path()
    );

    format!("{}\n{}", version_section, request_section)
}

async fn get_job(
    body: Request,
    queue: Arc<Mutex<HashMap<u32, Mutex<Job>>>>,
) -> Result<Json<Job>, StatusCode> {
    let value: anyhow::Result<u32> = try {
        body.headers()
            .get("job_id")
            .ok_or(OptimizationServerError::GetJob)?
            .to_str()?
            .parse()?
    };

    if let Ok(o) = value {
        match queue.lock().await.get(&o) {
            Some(o) => Ok(Json(o.lock().await.clone())),
            None => Err(StatusCode::BAD_REQUEST),
        }
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

async fn get_file(body: Request, queue: Arc<Mutex<HashMap<u32, Mutex<Job>>>>) -> impl IntoResponse {
    let value: anyhow::Result<u32> = try {
        body.headers()
            .get("job_id")
            .ok_or(OptimizationServerError::GetJob)?
            .to_str()?
            .parse()?
    };

    if let Ok(o) = value {
        match queue.lock().await.get(&o) {
            Some(o) => {
                let job = o.lock().await.clone();

                let request = Request::new(Body::empty());

                Ok(ServeFile::new(job.output_path)
                    .try_call(request)
                    .await
                    .unwrap())
            }
            None => Err(StatusCode::NOT_FOUND),
        }
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

async fn add_job(
    extract::Json(body): extract::Json<JobRecieved>,
    queue: Arc<Mutex<HashMap<u32, Mutex<Job>>>>,
    send_channel: Sender<u32>,
) -> Response {
    debug!("Optimization request for: {:?}", body.url);

    let jellyfin_url = env::var("JELLYFIN_URL");

    let mut url = body.url;

    if let Ok(o) = jellyfin_url {
        url = (*REGEX.replacen(&url, 1, &o)).to_string();
    }

    let id = random::<u32>();

    let job = Job {
        id,
        status: JobStatus::Queued,
        progress: 0,
        output_path: format!("files/{}.mp4", id),
        device_id: body.device_id,
        file_extension: String::from("mp4"),
        input_url: url,
        item: body.item,
        timestamp: Utc::now(),
        size: 0,
    };
    trace!("Added job: {:?}", job);

    queue.lock().await.insert(id, Mutex::new(job));
    let sent = send_channel.send(id).await;

    match sent {
        Ok(_) => Response::builder()
            .status(StatusCode::CREATED)
            .header("JOB_ID", id)
            .body(Body::from("Job Created"))
            .unwrap(),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("Error when creating job"))
            .unwrap(),
    }
}

async fn handle_jobs(
    queue: Arc<Mutex<HashMap<u32, Mutex<Job>>>>,
    recieve_channel: Arc<Mutex<Receiver<u32>>>,
) {
    loop {
        if let Some(j) = recieve_channel.lock().await.recv().await {
            println!("Recieved job with ID: {}", j);

            let entry_lock = queue.lock().await;
            let entry = entry_lock.get(&j).unwrap();
            entry.lock().await.status = JobStatus::Optimizing;
            let info = entry.lock().await.clone();

            drop(entry_lock);

            println!("Job Info is: {:?}", info);

            let input_file = CString::new(info.input_url.clone()).unwrap();
            let output_file = CString::new(info.output_path.clone()).unwrap();

            match remux(&input_file, &output_file) {
                Ok(_) => {
                    let entry_lock = queue.lock().await;
                    let entry = entry_lock.get(&j).unwrap();
                    entry.lock().await.status = JobStatus::Completed;
                    entry.lock().await.size =
                        Path::new(&info.output_path).metadata().unwrap().size();
                }
                Err(_) => {
                    let entry_lock = queue.lock().await;
                    let entry = entry_lock.get(&j).unwrap();
                    entry.lock().await.status = JobStatus::Failed;
                }
            }
        }
        sleep(Duration::new(5, 0));
    }
}
