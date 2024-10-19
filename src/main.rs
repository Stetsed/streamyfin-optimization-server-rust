use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs::File,
    path::Path,
    sync::{Arc, LazyLock},
    thread::sleep,
    time::Duration,
};

use axum::{
    extract::{self, Request},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};

use chrono::{DateTime, Utc};
use ffmpeg_next::{codec, encoder, format, log, media, Rational};
use rand::random;
use regex::Regex;
use serde_derive::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex,
};
use tracing::{debug, info, instrument, level_filters::LevelFilter, trace};
use tracing_subscriber::{filter::EnvFilter, util::SubscriberInitExt};
use tracing_subscriber::{fmt, layer::SubscriberExt};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_JOB_QUEUE: usize = 64;

static REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^.+/videos").unwrap());

#[derive(Error, Debug)]
pub enum OptimizationServerError {}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct JobRecieved {
    pub url: String,
    pub file_extension: String,
    pub device_id: String,
    pub item_id: String,
    pub item: String,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord)]
pub struct Job {
    pub id: u32,
    pub status: JobStatus,
    progress: u32,
    output_path: OsString,
    input_url: String,
    device_id: String,
    timestamp: DateTime<Utc>,
    file_extension: String,
    size: u32,
    item: String,
    // speed: u32 (Was in original implementation??? Ig to set ffmpeg speed maybe?)
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord)]
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
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    let ip: String = env::var("APPLICATION_IP").unwrap_or("0.0.0.0".to_string());

    let port: String = env::var("APPLICATION_PORT").unwrap_or("3000".to_string());

    let (send_channel, recieve_channel): (Sender<u32>, Receiver<u32>) =
        mpsc::channel(MAX_JOB_QUEUE);

    let recieve_channel_safe = Arc::new(Mutex::new(recieve_channel));

    let job_queue_arc: Arc<Mutex<HashMap<u32, Mutex<Job>>>> = Arc::new(Mutex::new(HashMap::new()));

    let cloned = Arc::clone(&job_queue_arc);
    tokio::spawn(async move { handle_jobs(cloned, Arc::clone(&recieve_channel_safe)).await });

    let cloned = Arc::clone(&job_queue_arc);
    let application_router: Router = Router::new().route("/info", get(info)).route(
        "/add-job",
        post(move |body: Json<JobRecieved>| add_job(body, cloned, send_channel)),
    );

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", ip, port))
        .await
        .unwrap();

    info!(
        "Started the Optimization Server Version: {}, bound to {}:{}",
        VERSION, ip, port
    );

    axum::serve(listener, application_router).await.unwrap();
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

#[instrument]
async fn add_job(
    extract::Json(body): extract::Json<JobRecieved>,
    queue: Arc<Mutex<HashMap<u32, Mutex<Job>>>>,
    send_channel: Sender<u32>,
) -> StatusCode {
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
        output_path: format!("files/{}.mp4", id).into(),
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
        Ok(_) => StatusCode::CREATED,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[instrument]
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

            let input_file = Path::new(&info.input_url);
            let output_file = Path::new(&info.output_path);

            ffmpeg_next::init().unwrap();

            let mut ictx = format::input(input_file).unwrap();

            let mut octx = format::output_as(output_file, &info.file_extension).unwrap();

            let mut stream_mapping = vec![0; ictx.nb_streams() as _];
            let mut ist_time_bases = vec![Rational(0, 1); ictx.nb_streams() as _];
            let mut ost_index = 0;
            for (ist_index, ist) in ictx.streams().enumerate() {
                let ist_medium = ist.parameters().medium();
                if ist_medium != media::Type::Audio
                    && ist_medium != media::Type::Video
                    && ist_medium != media::Type::Subtitle
                {
                    stream_mapping[ist_index] = -1;
                    continue;
                }
                stream_mapping[ist_index] = ost_index;
                ist_time_bases[ist_index] = ist.time_base();
                ost_index += 1;
                let mut ost = octx.add_stream(encoder::find(codec::Id::MPEG4)).unwrap();
                ost.set_parameters(ist.parameters());
                // We need to set codec_tag to 0 lest we run into incompatible codec tag
                // issues when muxing into a different container format. Unfortunately
                // there's no high level API to do this (yet).
                unsafe {
                    (*ost.parameters().as_mut_ptr()).codec_tag = 0;
                }
            }

            octx.set_metadata(ictx.metadata().to_owned());
            octx.write_header().unwrap();

            for (stream, mut packet) in ictx.packets() {
                let ist_index = stream.index();
                let ost_index = stream_mapping[ist_index];
                if ost_index < 0 {
                    continue;
                }
                let ost = octx.stream(ost_index as _).unwrap();
                packet.rescale_ts(ist_time_bases[ist_index], ost.time_base());
                packet.set_position(-1);
                packet.set_stream(ost_index as _);
                packet.write_interleaved(&mut octx).unwrap();
            }
        }
        sleep(Duration::new(5, 0));
    }
}
