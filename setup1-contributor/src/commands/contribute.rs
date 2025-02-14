use crate::{
    cli::commands::contribute::ContributeOptions,
    errors::ContributeError,
    objects::LockResponse,
    setup_keys::{
        confirmation_key::{print_key_and_remove_the_file, ConfirmationKey},
        AleoSetupKeys,
    },
    tasks::Tasks,
    utils::{
        create_parameters_for_chunk,
        get_authorization_value,
        read_from_file,
        remove_file_if_exists,
        sign_contribution_state,
        UploadMode,
    },
};

use phase1::helpers::converters::CurveKind;
use phase1_cli::contribute;
use phase1_coordinator::{
    environment::Environment,
    objects::{Chunk, Participant, Round},
};
use setup1_shared::structures::{PublicSettings, TwitterInfo};
use setup_utils::calculate_hash;
use snarkvm_curves::{bls12_377::Bls12_377, bw6_761::BW6_761, PairingEngine};
use snarkvm_dpc::{parameters::testnet2::Testnet2Parameters, Address, PrivateKey, ViewKey};

use age::DecryptError;
use anyhow::{Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use indicatif::{ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use panic_control::{spawn_quiet, ThreadResultExt};
use rand::{CryptoRng, Rng};
use regex::Regex;
use secrecy::{ExposeSecret, SecretString, SecretVec};
use setup_utils::derive_rng_from_seed;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    convert::TryFrom,
    fs::File,
    io::{Read, Write},
    ops::Deref,
    path::Path,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::{sleep, Instant};
use tracing::{debug, error, info, warn};
use url::Url;

const CHALLENGE_FILENAME: &str = "challenge";
const CHALLENGE_HASH_FILENAME: &str = "challenge.hash";
const RESPONSE_FILENAME: &str = "response";
const RESPONSE_HASH_FILENAME: &str = "response.hash";

const DELAY_AFTER_ERROR: Duration = Duration::from_secs(60);
const DELAY_WAIT_FOR_PIPELINE: Duration = Duration::from_secs(5);
const DELAY_POLL_CEREMONY: Duration = Duration::from_secs(5);
const HEARTBEAT_POLL_DELAY: Duration = Duration::from_secs(30);

lazy_static! {
    static ref PIPELINE: RwLock<HashMap<PipelineLane, VecDeque<LockResponse>>> = {
        let mut map = HashMap::new();
        map.insert(PipelineLane::Download, VecDeque::new());
        map.insert(PipelineLane::Process, VecDeque::new());
        map.insert(PipelineLane::Upload, VecDeque::new());
        RwLock::new(map)
    };
    static ref TASKS: RwLock<Tasks> = RwLock::new(Tasks::default());
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PipelineLane {
    Download,
    Process,
    Upload,
}

impl std::fmt::Display for PipelineLane {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Clone)]
pub struct Contribute {
    pub server_url: Url,
    /// Public key id for this contributor: e.g.
    /// `aleo1h7pwa3dh2egahqj7yvq7f7e533lr0ueysaxde2ktmtu2pxdjvqfqsj607a`
    pub participant_id: Address<Testnet2Parameters>,
    pub private_key: PrivateKey<Testnet2Parameters>,
    seed: Arc<SecretVec<u8>>,
    pub upload_mode: UploadMode,
    pub environment: Environment,

    pub challenge_filename: String,
    pub challenge_hash_filename: String,
    pub response_filename: String,
    pub response_hash_filename: String,

    pub max_in_download_lane: usize,
    pub max_in_process_lane: usize,
    pub max_in_upload_lane: usize,
    pub disable_pipelining: bool,

    pub current_task: Option<LockResponse>,
}

impl Contribute {
    pub fn new(
        opts: &ContributeOptions,
        environment: &Environment,
        private_key: PrivateKey<Testnet2Parameters>,
        seed: SecretVec<u8>,
    ) -> Self {
        // TODO (raychu86): Pass in pipelining options from the CLI.

        Self {
            server_url: opts.api_url.clone(),
            participant_id: Address::try_from(&private_key).expect("Should have derived an Aleo address"),
            private_key,
            seed: Arc::new(seed),
            upload_mode: opts.upload_mode.clone(),
            environment: environment.clone(),

            challenge_filename: CHALLENGE_FILENAME.to_string(),
            challenge_hash_filename: CHALLENGE_HASH_FILENAME.to_string(),
            response_filename: RESPONSE_FILENAME.to_string(),
            response_hash_filename: RESPONSE_HASH_FILENAME.to_string(),

            max_in_download_lane: 1,
            max_in_process_lane: 1,
            max_in_upload_lane: 1,
            disable_pipelining: false,

            current_task: None,
        }
    }

    pub fn clone_with_new_filenames(&self, index: usize) -> Self {
        let mut cloned = self.clone();
        cloned.challenge_filename = format!("{}_{}", self.challenge_filename, index);
        cloned.challenge_hash_filename = format!("{}_{}", self.challenge_hash_filename, index);
        cloned.response_filename = format!("{}_{}", self.response_filename, index);
        cloned.response_hash_filename = format!("{}_{}", self.response_hash_filename, index);
        cloned
    }

    async fn run_and_catch_errors<E: PairingEngine>(&self) -> Result<()> {
        let progress_bar = ProgressBar::new(0);
        let progress_style =
            ProgressStyle::default_bar().template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}");
        progress_bar.enable_steady_tick(1000);
        progress_bar.set_style(progress_style);
        progress_bar.set_message("Getting initial data from the server...");

        let join_result = self.join_queue(&mut rand::thread_rng()).await;
        match join_result {
            Ok(joined) => {
                info!("Attempting to join the queue with response: {}", joined);
                if !joined {
                    // it means contributor either already contributed,
                    // or has a low reliability score, or unable to
                    // join the queue
                    return Err(anyhow::anyhow!("Queue join returned false"));
                }
            }
            Err(err) => {
                let text = format!("Failed to join the queue, error: {}", err);
                error!("{}", text);
                return Err(anyhow::anyhow!("{}", text));
            }
        }

        // Get total number of tokio tasks to generate.
        let n_concurrent_tasks = match self.disable_pipelining {
            true => 1,
            false => self.max_in_download_lane + self.max_in_process_lane + self.max_in_upload_lane,
        };

        // Run status bar updater.
        let updater = StatusUpdater {
            server_url: self.server_url.clone(),
            participant_id: self.participant_id.to_string(),
        };
        let update_handle = update_progress_bar(updater, progress_bar.clone());

        // Push the handle for the progress bar here, so it fully exits before
        // we start printing stuff at the end of the round.
        let mut futures = vec![update_handle];

        for i in 0..n_concurrent_tasks {
            let mut cloned_contribute = self.clone_with_new_filenames(i);

            let join_handle = tokio::task::spawn(async move {
                // Run contributor loop.
                loop {
                    let result = cloned_contribute.run::<E>().await;
                    match result {
                        Ok(_) => {
                            info!("Successfully contributed, thank you for participation!");
                            break;
                        }
                        Err(err) => {
                            println!("Got error from run: {}, retrying...", err);
                            tracing::error!("Error from contribution run: {}", err);

                            if let Some(lock_response) = cloned_contribute.current_task.as_ref() {
                                tracing::warn!("Retrying task {:?}", lock_response);
                                cloned_contribute
                                    .remove_task_from_lane_if_exists(&PipelineLane::Download, &lock_response)
                                    .expect("Should have removed task from download lane");

                                cloned_contribute
                                    .remove_task_from_lane_if_exists(&PipelineLane::Upload, &lock_response)
                                    .expect("Should have removed task from upload lane");

                                cloned_contribute
                                    .remove_task_from_lane_if_exists(&PipelineLane::Process, &lock_response)
                                    .expect("Should have removed task from process lane");

                                cloned_contribute.add_task_to_queue(lock_response.clone());
                            }
                        }
                    }

                    sleep(DELAY_AFTER_ERROR).await;
                }
            });
            futures.push(join_handle);
            sleep(DELAY_WAIT_FOR_PIPELINE).await;
        }

        // NOTE: Attempted to use a task, however this did not work as
        // epected. It seems likely there is some blocking code in one
        // of the other tasks.
        initiate_heartbeat(self.server_url.clone(), self.private_key.clone());

        futures::future::try_join_all(futures).await?;

        print_key_and_remove_the_file().expect("Error finalizing the participation");

        // Let's see if the contributor wants to log an ETH address for their NFT
        if let Err(e) = self.prompt_eth_address(&mut rand::rngs::OsRng).await {
            tracing::error!("Error while prompting for ETH address - {}", e);
        }

        match self.prompt_tweet(&mut rand::rngs::OsRng).await {
            Ok(s) => {
                if s.is_some() {
                    println!("Thanks for making an attestation! Your tweet: {}", s.unwrap());
                }
            }
            Err(e) => {
                tracing::error!("Error while prompting for an attestation tweet - {}", e);
            }
        };

        Ok(())
    }

    ///
    /// The function attempts to fetch a task from the queue. If there are
    /// no tasks in the queue or the task is already complete, then return `None`
    ///
    #[inline]
    pub async fn get_task_from_queue(&self, ceremony: &Round) -> Result<Option<LockResponse>> {
        // Acquire the tasks lock.
        let mut tasks = TASKS.write().expect("Should have opened queue for writing");

        let task = tasks.next_task();

        // Return `None` if the participant doesn't hold the lock on the chunk.
        if let Some(task) = &task {
            if !self.is_pending_task(&task, ceremony)? {
                return Ok(None);
            }
        }

        Ok(task)
    }

    ///
    /// Add a task to the queue.
    ///
    #[inline]
    pub fn add_task_to_queue(&self, task: LockResponse) {
        // Acquire the tasks lock.
        let mut tasks = TASKS.write().expect("Should have opened queue for writing");

        tasks.add_task(task);
    }

    ///
    /// Remove a task from the queue.
    ///
    #[inline]
    pub async fn remove_task_from_queue(&self, task: &LockResponse) -> Result<()> {
        // Acquire the tasks lock.
        let mut tasks = TASKS.write().expect("Should have opened queue for writing");

        // Remove the given task from `tasks`.
        tasks.remove_task(task);

        Ok(())
    }

    async fn wait_for_available_spot_in_lane(&self, lane: &PipelineLane) -> Result<()> {
        let max_in_lane = match *lane {
            PipelineLane::Download => self.max_in_download_lane,
            PipelineLane::Process => self.max_in_process_lane,
            PipelineLane::Upload => self.max_in_upload_lane,
        };
        loop {
            {
                let pipeline = PIPELINE.read().expect("Should have opened pipeline for reading");
                if pipeline
                    .get(lane)
                    .ok_or(ContributeError::LaneWasNullError(lane.to_string()))?
                    .len()
                    < max_in_lane
                {
                    return Ok(());
                }
            }
            sleep(DELAY_WAIT_FOR_PIPELINE).await;
        }
    }

    fn move_task_from_lane_to_lane(&self, from: &PipelineLane, to: &PipelineLane, task: &LockResponse) -> Result<bool> {
        let max_in_lane = match *to {
            PipelineLane::Download => self.max_in_download_lane,
            PipelineLane::Process => self.max_in_process_lane,
            PipelineLane::Upload => self.max_in_upload_lane,
        };
        {
            let mut pipeline = PIPELINE.write().expect("Should have opened pipeline for writing");

            // Check that the `to` pipeline has enough space.
            {
                let to_list = pipeline
                    .get_mut(to)
                    .ok_or(ContributeError::LaneWasNullError(to.to_string()))?;

                if to_list.len() >= max_in_lane {
                    return Ok(false);
                }
            }

            // Check that the tasks exists in the `from` pipeline.
            {
                let from_list = pipeline
                    .get_mut(from)
                    .ok_or(ContributeError::LaneWasNullError(from.to_string()))?;
                if !from_list.contains(&task) {
                    return Err(ContributeError::LaneDidNotContainChunkWithIDError(
                        from.to_string(),
                        task.chunk_id.to_string(),
                    )
                    .into());
                }
                from_list.retain(|c| c != task);
            }

            // Add the task to the `to` pipeline.
            {
                let to_list = pipeline
                    .get_mut(to)
                    .ok_or(ContributeError::LaneWasNullError(to.to_string()))?;

                if to_list.contains(&task) {
                    return Err(ContributeError::LaneAlreadyContainsChunkWithIDError(
                        to.to_string(),
                        task.chunk_id.to_string(),
                    )
                    .into());
                }
                to_list.push_back(task.clone());
            }
            debug!(
                "Chunk ID {} moved successfully from lane {} to lane {}. Current pipeline is: {:#?}\n",
                task.chunk_id,
                from,
                to,
                pipeline.deref()
            );
            Ok(true)
        }
    }

    async fn wait_and_move_task_from_lane_to_lane(
        &self,
        from: &PipelineLane,
        to: &PipelineLane,
        task: &LockResponse,
    ) -> Result<()> {
        loop {
            match self.move_task_from_lane_to_lane(from, to, task)? {
                true => return Ok(()),
                false => sleep(DELAY_WAIT_FOR_PIPELINE).await,
            }
        }
    }

    async fn wait_and_add_task_to_download_lane(&self, task: &LockResponse) -> Result<()> {
        loop {
            match self.add_task_to_download_lane(task)? {
                true => return Ok(()),
                false => sleep(DELAY_WAIT_FOR_PIPELINE).await,
            }
        }
    }

    fn add_task_to_download_lane(&self, task: &LockResponse) -> Result<bool> {
        let lane = &PipelineLane::Download;
        let mut pipeline = PIPELINE.write().expect("Should have opened pipeline for writing");

        let lane_list = pipeline
            .get_mut(lane)
            .ok_or(ContributeError::LaneWasNullError(lane.to_string()))?;

        if lane_list.contains(&task) || lane_list.len() >= self.max_in_download_lane {
            return Ok(false);
        }
        lane_list.push_back(task.clone());
        debug!(
            "Chunk ID {} added successfully to lane {}. Current pipeline is: {:#?}",
            task.chunk_id,
            lane,
            pipeline.deref()
        );
        Ok(true)
    }

    fn remove_task_from_lane_if_exists(&self, lane: &PipelineLane, task: &LockResponse) -> Result<bool> {
        let mut pipeline = PIPELINE.write().expect("Should have opened pipeline for writing");

        let lane_list = pipeline
            .get_mut(lane)
            .ok_or(ContributeError::LaneWasNullError(lane.to_string()))?;
        if !lane_list.contains(&task) {
            return Ok(false);
        }
        lane_list.retain(|c| c != task);
        debug!(
            "Chunk ID {} removed successfully from lane {}... Current pipeline is: {:#?}\n",
            task.chunk_id,
            lane,
            pipeline.deref()
        );
        Ok(true)
    }

    async fn run<E: PairingEngine>(&mut self) -> Result<()> {
        loop {
            self.wait_for_available_spot_in_lane(&PipelineLane::Download).await?;
            let auth_rng = &mut rand::rngs::OsRng;

            let ceremony = self.get_ceremony().await?;
            let non_contributed_chunks = get_non_contributed_chunks(&ceremony, &self.participant_id.to_string());
            let incomplete_chunks = self.get_non_contributed_and_available_chunks(&ceremony);

            // Check if the contributor is finished or needs to wait for an available lock
            if incomplete_chunks.len() == 0 {
                if non_contributed_chunks.len() == 0 {
                    println!("You have completed your contribution! Thank you!");
                    remove_file_if_exists(&self.challenge_filename)?;
                    remove_file_if_exists(&self.challenge_hash_filename)?;
                    remove_file_if_exists(&self.response_filename)?;
                    remove_file_if_exists(&self.response_hash_filename)?;
                    return Ok(());
                } else {
                    tokio::time::sleep(DELAY_POLL_CEREMONY).await;
                    continue;
                }
            }

            // Attempt to fetch a task from the queue or lock a chunk from the coordinator.
            let lock_response = match self.get_task_from_queue(&ceremony).await? {
                Some(lock_response) => lock_response,
                None => self.lock_chunk(auth_rng).await?,
            };

            // Add the lock response to the download lane
            self.wait_and_add_task_to_download_lane(&lock_response).await?;

            self.current_task = Some(lock_response.clone());
            let chunk_id = lock_response.chunk_id;

            remove_file_if_exists(&self.challenge_filename)?;
            remove_file_if_exists(&self.challenge_hash_filename)?;
            self.download_challenge(
                chunk_id,
                lock_response.contribution_id,
                &self.challenge_filename,
                auth_rng,
            )
            .await?;

            // Wait for the process pipeline to open up
            self.wait_and_move_task_from_lane_to_lane(&PipelineLane::Download, &PipelineLane::Process, &lock_response)
                .await?;

            let exposed_seed = self.seed.expose_secret();
            let seeded_rng = derive_rng_from_seed(&exposed_seed[..]);
            let start = Instant::now();
            remove_file_if_exists(&self.response_filename)?;
            remove_file_if_exists(&self.response_hash_filename)?;

            // Fetch parameters required for contribution.
            let parameters = create_parameters_for_chunk::<E>(&self.environment, chunk_id as usize)?;
            let compressed_input = self.environment.compressed_inputs();
            let compressed_output = self.environment.compressed_outputs();
            let check_input_correctness = self.environment.check_input_for_correctness();

            let challenge_filename = self.challenge_filename.to_string();
            let response_filename = self.response_filename.to_string();

            // Run the contribution.
            let h = spawn_quiet(move || {
                contribute(
                    compressed_input,
                    &challenge_filename,
                    compressed_output,
                    &response_filename,
                    check_input_correctness,
                    &parameters,
                    seeded_rng,
                );
            });
            let result = h.join();
            if !result.is_ok() {
                if let Some(panic_value) = result.panic_value_as_str() {
                    error!("Contribute failed: {}", panic_value);
                }
                return Err(ContributeError::FailedRunningContributeError.into());
            }
            let duration = start.elapsed();

            info!("Completed chunk {} in {} seconds", chunk_id, duration.as_secs());

            // Read the challenge and response files.
            let challenge_file = read_from_file(&self.challenge_filename)?;
            let response_file = read_from_file(&self.response_filename)?;

            // Hash the challenge and response files.
            let challenge_hash = calculate_hash(&challenge_file).to_vec();
            let response_hash = calculate_hash(&response_file).to_vec();

            // Sign the contribution state.
            let view_key = ViewKey::try_from(&self.private_key)?;
            let signed_contribution_state =
                sign_contribution_state(&view_key.to_string(), &challenge_hash, &response_hash, None, auth_rng)?;

            // Construct the serialized response
            let mut file = File::open(&self.response_filename)?;
            let mut response_file = Vec::new();
            file.read_to_end(&mut response_file)?;

            // Concatenate the signed contribution data and next challenge file.
            let verifier_flag = vec![0];
            let signature_bytes = hex::decode(signed_contribution_state.get_signature())?;

            let signature_and_response_file_bytes = [
                verifier_flag,
                signature_bytes,
                challenge_hash,
                response_hash,
                response_file,
            ]
            .concat();

            // Wait for the Upload pipeline to open up
            self.wait_and_move_task_from_lane_to_lane(&PipelineLane::Process, &PipelineLane::Upload, &lock_response)
                .await?;

            let upload_url = &lock_response.response_locator;

            // Upload the response and contribution file signature to the coordinator.
            match self.upload_mode {
                UploadMode::Auto => {
                    if upload_url.contains("blob.core.windows.net") {
                        self.upload_response(
                            lock_response.response_chunk_id,
                            lock_response.response_contribution_id,
                            signature_and_response_file_bytes,
                            auth_rng,
                        )
                        .await?;
                    } else {
                        self.upload_response(
                            lock_response.response_chunk_id,
                            lock_response.response_contribution_id,
                            signature_and_response_file_bytes,
                            auth_rng,
                        )
                        .await?;
                    }
                }
                UploadMode::Direct => {
                    self.upload_response(
                        lock_response.response_chunk_id,
                        lock_response.response_contribution_id,
                        signature_and_response_file_bytes,
                        auth_rng,
                    )
                    .await?
                }
            }

            // Attempt to perform the contribution with the uploaded response file at the `upload_url`.
            self.notify_contribution(chunk_id, serde_json::json!({}), auth_rng)
                .await?;

            // Remove the task from the upload pipeline.
            self.remove_task_from_lane_if_exists(&PipelineLane::Upload, &lock_response)?;

            // Remove the task from the queue
            self.remove_task_from_queue(&lock_response).await?;
        }
    }

    /// Returns `true` if the participant currently holds the lock on the chunk.
    fn is_pending_task(&self, task: &LockResponse, ceremony: &Round) -> Result<bool> {
        if let Some(chunk) = ceremony.chunks().get(task.chunk_id as usize) {
            if chunk.is_locked_by(&Participant::Contributor(self.participant_id.to_string())) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Get references to the unlocked chunks which have been
    /// completely verified, and do not yet contain a contribution
    /// from this contributor.
    fn get_non_contributed_and_available_chunks<'r>(&self, ceremony: &'r Round) -> Vec<&'r Chunk> {
        get_non_contributed_chunks(ceremony, &self.participant_id.to_string())
            .into_iter()
            .filter(|chunk| chunk.lock_holder().is_none())
            .collect()
    }

    async fn join_queue<R: Rng + CryptoRng>(&self, auth_rng: &mut R) -> Result<bool> {
        let join_queue_path = "/v1/queue/contributor/join";
        let join_queue_path_url = self.server_url.join(&join_queue_path)?;
        let client = reqwest::Client::new();
        let authorization = get_authorization_value(&self.private_key, "POST", &join_queue_path, auth_rng)?;

        let address = self.participant_id.to_string();
        let confirmation_key = ConfirmationKey::for_current_round(address)?;
        let bytes = serde_json::to_vec(&confirmation_key)?;

        let response = client
            .post(join_queue_path_url.as_str())
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_LENGTH, bytes.len())
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;

        let data = response.bytes().await?;
        let joined = serde_json::from_slice::<bool>(&*data)?;

        Ok(joined)
    }

    async fn get_ceremony(&self) -> Result<Round> {
        let ceremony_url = self.server_url.join("/v1/round/current")?;
        let response = reqwest::get(ceremony_url.as_str()).await?.error_for_status()?;

        let data = response.bytes().await?;
        let ceremony: Round = serde_json::from_slice(&*data)?;

        Ok(ceremony)
    }

    async fn lock_chunk<R: Rng + CryptoRng>(&self, auth_rng: &mut R) -> Result<LockResponse> {
        let lock_path = "/v1/contributor/try_lock";
        let lock_chunk_url = self.server_url.join(&lock_path)?;
        let client = reqwest::Client::new();
        let authorization = get_authorization_value(&self.private_key, "POST", &lock_path, auth_rng)?;
        let response = client
            .post(lock_chunk_url.as_str())
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_LENGTH, 0)
            .send()
            .await?
            .error_for_status()?;

        let data = response.bytes().await?;
        let lock_response = serde_json::from_slice::<LockResponse>(&*data)?;

        Ok(lock_response)
    }

    async fn download_challenge<R: Rng + CryptoRng>(
        &self,
        chunk_id: u64,
        contribution_id: u64,
        file_path: &str,
        auth_rng: &mut R,
    ) -> Result<()> {
        let download_path = format!("/v1/download/challenge/{}/{}", chunk_id, contribution_id);
        let download_path_url = self.server_url.join(&download_path)?;
        let client = reqwest::Client::new();
        let authorization = get_authorization_value(&self.private_key, "GET", &download_path, auth_rng)?;
        let mut response = client
            .get(download_path_url.as_str())
            .header(http::header::AUTHORIZATION, authorization)
            .send()
            .await?
            .error_for_status()?;

        remove_file_if_exists(file_path)?;
        let mut out = File::create(file_path)?;
        while let Some(chunk) = response.chunk().await? {
            out.write_all(&chunk)?;
        }

        Ok(())
    }

    async fn upload_response<R: Rng + CryptoRng>(
        &self,
        chunk_id: u64,
        contribution_id: u64,
        contents: Vec<u8>,
        auth_rng: &mut R,
    ) -> Result<()> {
        let upload_path = format!("/v1/upload/response/{}/{}", chunk_id, contribution_id);
        let upload_path_url = self.server_url.join(&upload_path)?;
        let client = reqwest::Client::new();
        let authorization = get_authorization_value(&self.private_key, "POST", &upload_path, auth_rng)?;
        client
            .post(upload_path_url.as_str())
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_TYPE, "application/octet-stream")
            .header(http::header::CONTENT_LENGTH, contents.len())
            .body(contents)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    async fn notify_contribution<R: Rng + CryptoRng>(
        &self,
        chunk_id: u64,
        body: serde_json::Value,
        auth_rng: &mut R,
    ) -> Result<()> {
        let contribute_path = format!("/v1/contributor/try_contribute/{}", chunk_id);
        let contribute_chunk_url = self.server_url.join(&contribute_path)?;
        let client = reqwest::Client::new();
        let authorization = get_authorization_value(&self.private_key, "POST", &contribute_path, auth_rng)?;
        let bytes = serde_json::to_vec(&body)?;
        client
            .post(contribute_chunk_url.as_str())
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_LENGTH, bytes.len())
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn prompt_tweet<R: Rng + CryptoRng>(&self, auth_rng: &mut R) -> Result<Option<String>> {
        if Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Would you like to tweet an attestation to your contribution?")
            .interact()
            .unwrap()
        {
            let request_token = self.get_twitter_request_token(auth_rng).await?;
            let auth_url = egg_mode::auth::authorize_url(&request_token);

            println!(
                "Please visit this URL and authorize the Aleo Setup application to tweet on your behalf - {}\n\nWhen you're finished, the website will give you a PIN code. Please enter it in the input below.",
                auth_url
            );

            loop {
                let re = Regex::new(r"^[0-9]{6}$").unwrap();
                let pin: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Enter PIN")
                    .validate_with({
                        move |input: &String| -> Result<(), &str> {
                            if re.is_match(input) {
                                Ok(())
                            } else {
                                Err("The PIN should be 6 digits.")
                            }
                        }
                    })
                    .interact_text()
                    .unwrap();

                let info = TwitterInfo {
                    request_token: request_token.clone(),
                    pin,
                };

                match self.post_tweet(auth_rng, info).await {
                    Ok(link) => return Ok(Some(link)),
                    Err(e) => println!("Could not post tweet - {}\n\n Please try again", e),
                };
            }
        }

        Ok(None)
    }

    async fn prompt_eth_address<R: Rng + CryptoRng>(&self, auth_rng: &mut R) -> Result<()> {
        println!(
            "Thank you for participating in Aleo Setup. As a token of appreciation, we would like to send you a commemorative NFT. This NFT is procedurally generated, but represents your unique contribution. We hope it serves as a reminder of the important role that YOU played in bringing Aleo to life.\n\nPlease enter the Ethereum address where you would like to receive the NFT:"
        );

        // Validate address
        // NOTE: this only checks that the inputted string conforms to the same structure
        // as ETH addresses - it does not actually perform a checksum validation.
        // https://ethereum.stackexchange.com/a/40670
        let re = Regex::new(r"^(0x|0X){1}[0-9a-fA-F]{40}$").unwrap();

        let address: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Your ETH address")
            .allow_empty(true)
            .validate_with({
                let mut opt_out = false;
                move |input: &String| -> Result<(), &str> {
                    if re.is_match(input) || (opt_out && input.len() == 0) {
                        Ok(())
                    } else if input.len() == 0 {
                        opt_out = true;
                        Err("Leaving this field blank will prevent you from receiving your NFT. Are you sure?")
                    } else {
                        opt_out = false;
                        Err("This is not a valid Ethereum address.")
                    }
                }
            })
            .interact_text()
            .unwrap();

        if address.len() > 0 {
            self.upload_eth_address(auth_rng, address).await
        } else {
            Ok(())
        }
    }

    async fn upload_eth_address<R: Rng + CryptoRng>(&self, auth_rng: &mut R, address: String) -> Result<()> {
        let upload_path = "/v1/contributor/add_eth_address";
        let upload_endpoint_url = self.server_url.join(&upload_path)?;
        let authorization = get_authorization_value(&self.private_key, "POST", &upload_path, auth_rng)?;
        let client = reqwest::Client::new();
        let bytes = serde_json::to_string(&address)?;
        client
            .post(upload_endpoint_url)
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_LENGTH, bytes.len())
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn get_twitter_request_token<R: Rng + CryptoRng>(&self, auth_rng: &mut R) -> Result<egg_mode::KeyPair> {
        let get_path = "/v1/contributor/get_twitter_request_token";
        let get_endpoint_url = self.server_url.join(&get_path)?;
        let authorization = get_authorization_value(&self.private_key, "GET", &get_path, auth_rng)?;
        let client = reqwest::Client::new();
        let response = client
            .get(get_endpoint_url)
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_LENGTH, 0)
            .send()
            .await?
            .error_for_status()?;

        let data = response.bytes().await?;
        let request_token = serde_json::from_slice::<egg_mode::KeyPair>(&*data)?;

        Ok(request_token)
    }

    async fn post_tweet<R: Rng + CryptoRng>(&self, auth_rng: &mut R, info: TwitterInfo) -> Result<String> {
        let post_path = "/v1/contributor/post_tweet";
        let post_endpoint_url = self.server_url.join(&post_path)?;
        let authorization = get_authorization_value(&self.private_key, "GET", &post_path, auth_rng)?;
        let client = reqwest::Client::new();
        let bytes = serde_json::to_vec(&info)?;
        let response = client
            .post(post_endpoint_url)
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_LENGTH, bytes.len())
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;

        let data = response.bytes().await?;
        let request_token = serde_json::from_slice::<String>(&*data)?;

        Ok(request_token)
    }
}

fn update_progress_bar(updater: StatusUpdater, progress_bar: ProgressBar) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn(async move {
        loop {
            match updater.status_updater(progress_bar.clone()).await {
                Ok(_) => {
                    if progress_bar.is_finished() {
                        return;
                    }
                }
                Err(e) => {
                    warn!("Got error from updater: {}", e);
                    progress_bar.set_message(&format!("Could not update status: {}", e.to_string().trim()));
                }
            }
            sleep(DELAY_POLL_CEREMONY).await;
        }
    })
}

/// Utility structure to provide updates about the round progress
struct StatusUpdater {
    server_url: Url,
    participant_id: String,
}

impl StatusUpdater {
    async fn status_updater(&self, progress_bar: ProgressBar) -> Result<()> {
        let ceremony = get_ceremony(&self.server_url).await?;
        let number_of_chunks = ceremony.chunks().len();

        progress_bar.set_length(number_of_chunks as u64);
        let non_contributed_chunks = get_non_contributed_chunks(&ceremony, &self.participant_id);

        let participant_locked_chunks = get_participant_locked_chunks_display(&ceremony, &self.participant_id)?;
        if participant_locked_chunks.len() > 0 {
            progress_bar.set_message(&format!(
                "Contributing to {} {}...",
                if participant_locked_chunks.len() > 1 {
                    "chunks"
                } else {
                    "chunk"
                },
                participant_locked_chunks.join(", "),
            ));
            progress_bar.set_position((number_of_chunks - non_contributed_chunks.len()) as u64);
        } else if non_contributed_chunks.len() == 0 {
            let completed_message = "Successfully contributed, thank you for participation! Waiting to see if you're still needed... Don't turn this off!";

            progress_bar.finish_with_message(completed_message);
            info!(completed_message);
        } else {
            progress_bar.set_position((number_of_chunks - non_contributed_chunks.len()) as u64);
            progress_bar.set_message(&format!("Waiting for an available chunk..."));
        }

        Ok(())
    }
}

async fn get_ceremony(server_url: &Url) -> Result<Round> {
    let ceremony_url = server_url.join("/v1/round/current")?;
    let response = reqwest::get(ceremony_url.as_str()).await?.error_for_status()?;

    let data = response.bytes().await?;
    let ceremony: Round = serde_json::from_slice(&*data)?;

    Ok(ceremony)
}

/// Get references to the chunks which have been completely
/// verified, and do not yet contain a contribution from this
/// contributor.
fn get_non_contributed_chunks<'r>(ceremony: &'r Round, participant_id: &String) -> Vec<&'r Chunk> {
    ceremony
        .chunks()
        .iter()
        .filter_map(|chunk| {
            if !chunk_all_verified(chunk) {
                return None;
            }

            if !contributor_ids_in_chunk(chunk).contains(participant_id) {
                Some(chunk)
            } else {
                None
            }
        })
        .collect()
}

fn get_participant_locked_chunks_display(ceremony: &Round, participant_id: &String) -> Result<Vec<String>> {
    let mut chunk_ids = vec![];

    for chunk in ceremony.chunks().iter() {
        let chunk_id = chunk.chunk_id();
        let chunk_lock_holder = chunk.lock_holder();

        if chunk_lock_holder.is_some()
            && chunk_lock_holder
                .as_ref()
                .map(|c| c.to_string().split('.').collect::<Vec<_>>()[0].to_string())
                == Some(participant_id.clone())
        {
            chunk_ids.push(format!("{}", chunk_id));
        }
    }

    Ok(chunk_ids)
}

struct HeartbeatData {
    server_url: Url,
    private_key: PrivateKey<Testnet2Parameters>,
}

impl HeartbeatData {
    async fn heartbeat<R: Rng + CryptoRng>(&self, auth_rng: &mut R) -> Result<()> {
        let heartbeat_path = "/v1/contributor/heartbeat";
        let url = self.server_url.join(&heartbeat_path)?;
        let client = reqwest::Client::new();
        let authorization = get_authorization_value(&self.private_key, "POST", &heartbeat_path, auth_rng)?;
        let response = client
            .post(url.as_str())
            .header(http::header::AUTHORIZATION, authorization)
            .header(http::header::CONTENT_LENGTH, 0)
            .send()
            .await?
            .error_for_status()?;

        response.error_for_status()?;

        Ok(())
    }
}

fn initiate_heartbeat(server_url: Url, private_key: PrivateKey<Testnet2Parameters>) {
    let private_key = private_key.to_string();
    std::thread::spawn(move || {
        let heartbeat_data = HeartbeatData {
            server_url,
            private_key: PrivateKey::from_str(&private_key).expect("Failed to create PrivateKey from String"),
        };

        let auth_rng = &mut rand::rngs::OsRng;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        loop {
            tracing::info!("Performing heartbeat.");
            if let Err(error) = runtime.block_on(heartbeat_data.heartbeat(auth_rng)) {
                tracing::error!("Error performing heartbeat: {}", error);
            }
            std::thread::sleep(HEARTBEAT_POLL_DELAY);
        }
    });
}

fn decrypt(passphrase: &SecretString, encrypted: &str) -> Result<Vec<u8>> {
    let decoded = SecretVec::new(hex::decode(encrypted)?);
    let decryptor = age::Decryptor::new(decoded.expose_secret().as_slice())?;
    let mut output = vec![];
    if let age::Decryptor::Passphrase(decryptor) = decryptor {
        let mut reader = decryptor
            .decrypt(passphrase, None)
            .map_err(|decrypt_error: DecryptError| match decrypt_error {
                DecryptError::ExcessiveWork { .. } => anyhow::Error::from(decrypt_error)
                    .context("Perhaps you have forgotten to compile in release mode, or your hardware is too slow?"),
                _ => anyhow::Error::from(decrypt_error),
            })
            .context("Unable to create decrypt reader")?;

        reader.read_to_end(&mut output)?;
    } else {
        return Err(ContributeError::UnsupportedDecryptorError.into());
    }

    Ok(output)
}

/// Decrypts and reads the private key from the specified `keys_path`,
/// decrypting using the specified `passphrase`
fn read_keys<P: AsRef<Path>>(
    keys_path: P,
    passphrase: &SecretString,
) -> Result<(SecretVec<u8>, PrivateKey<Testnet2Parameters>)> {
    let mut contents = String::new();
    std::fs::File::open(keys_path)?.read_to_string(&mut contents)?;
    let keys: AleoSetupKeys = serde_json::from_str(&contents)?;

    let seed = SecretVec::new(decrypt(passphrase, &keys.encrypted_seed)?);
    let decrypted_private_key = SecretVec::new(decrypt(passphrase, &keys.encrypted_private_key)?);
    let private_key = PrivateKey::from_str(std::str::from_utf8(decrypted_private_key.expose_secret())?)?;

    Ok((seed, private_key))
}

async fn request_coordinator_public_settings(coordinator_url: &Url) -> anyhow::Result<PublicSettings> {
    let settings_endpoint_url = coordinator_url.join("/v1/coordinator/settings")?;
    let client = reqwest::Client::new();
    let bytes = client
        .post(settings_endpoint_url)
        .header(http::header::CONTENT_LENGTH, 0)
        .send()
        .await?
        .bytes()
        .await?;
    PublicSettings::decode(&bytes.to_vec())
        .map_err(|e| anyhow::anyhow!("Error decoding coordinator PublicSettings: {}", e))
}

pub async fn contribute_subcommand(opts: &ContributeOptions) -> anyhow::Result<()> {
    let public_settings = request_coordinator_public_settings(&opts.api_url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch the coordinator public settings");
            e
        })
        .with_context(|| format!("Failed to fetch the coordinator public settings"))?;

    start_contributor(opts, &public_settings).await
}

async fn start_contributor(opts: &ContributeOptions, public_settings: &PublicSettings) -> Result<()> {
    let environment = crate::utils::environment_by_setup_kind(&public_settings.setup);

    // Initialize tracing logger. Stored to `aleo-setup.log`.
    let appender = tracing_appender::rolling::never(".", "aleo-setup.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt().with_writer(non_blocking).init();

    let passphrase = crate::setup_keys::read_passphrase(opts.passphrase.clone())?;

    // Read the stored contribution seed and Aleo private key.
    let (seed, private_key) = read_keys(&opts.keys_path, &passphrase).expect("Unable to load Aleo setup keys");

    let curve_kind = environment.parameters().curve();

    // Initialize the contributor.
    let contribute = Contribute::new(opts, &environment, private_key, seed);

    if public_settings.check_reliability {
        tracing::info!("Checking reliability score before joining the queue");
        crate::reliability::check(&opts.api_url, &contribute.private_key).await?;
        tracing::info!("Reliability checks completed successfully");
    }

    // Run the contributor.
    let contribution = match curve_kind {
        CurveKind::Bls12_377 => contribute.run_and_catch_errors::<Bls12_377>().await,
        CurveKind::BW6 => contribute.run_and_catch_errors::<BW6_761>().await,
    };

    match contribution {
        Err(e) => info!("Error occurred during contribution: {}", e.to_string()),
        _ => {}
    }

    Ok(())
}

/// Check that every contribution in the chunk has been verified.
fn chunk_all_verified(chunk: &Chunk) -> bool {
    chunk.get_contributions().iter().all(|(_, c)| c.is_verified())
}

/// Obtain a set with the id of every contributor in the chunk who's
/// contribution has been verified.
fn contributor_ids_in_chunk(chunk: &Chunk) -> HashSet<String> {
    chunk
        .get_contributions()
        .iter()
        .filter(|(_, c)| c.is_verified())
        .filter_map(|(_, c)| {
            c.get_contributor()
                .as_ref()
                .map(|c| c.to_string().split('.').collect::<Vec<_>>()[0].to_string())
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::{chunk_all_verified, contributor_ids_in_chunk};
    use phase1_coordinator::objects::{Chunk, Participant};

    #[test]
    fn test_participant_ids_in_chunk() {
        let verifier = Participant::Verifier(
            "aleo1yphn5z63acdpelyk2c3xmf6fuzpxymusp3c260ne6q0rrhrtdufqenlwqg.verifier".to_string(),
        );
        let contributor1 = Participant::Contributor(
            "aleo1fa6q44gpw0vkpx7xsfhgadz48swtg3wqf98w0xkrydwtvs62q5zsqyv5d7.contributor".to_string(),
        );
        let contributor2 = Participant::Contributor(
            "aleo1h7pwa3dh2egahqj7yvq7f7e533lr0ueysaxde2ktmtu2pxdjvqfqsj607a.contributor".to_string(),
        );

        let mut chunk = Chunk::new(0, verifier.clone(), String::new().into(), String::new().into()).unwrap();

        chunk.acquire_lock(contributor1.clone(), 3).unwrap();
        chunk
            .add_contribution(1, &contributor1, String::new().into(), String::new().into())
            .unwrap();
        assert!(!chunk_all_verified(&chunk));
        chunk.acquire_lock(verifier.clone(), 3).unwrap();
        chunk
            .verify_contribution(1, verifier.clone(), String::new().into(), String::new().into())
            .unwrap();
        assert!(chunk_all_verified(&chunk));

        chunk.acquire_lock(contributor2.clone(), 3).unwrap();
        chunk
            .add_contribution(2, &contributor2, String::new().into(), String::new().into())
            .unwrap();
        assert!(!chunk_all_verified(&chunk));
        chunk.acquire_lock(verifier.clone(), 3).unwrap();
        chunk
            .verify_contribution(2, verifier.clone(), String::new().into(), String::new().into())
            .unwrap();
        assert!(chunk_all_verified(&chunk));

        let ids = contributor_ids_in_chunk(&chunk);
        assert_eq!(2, ids.len());
        assert!(ids.contains(&contributor1.to_string().replace(".contributor", "")));
        assert!(ids.contains(&contributor2.to_string().replace(".contributor", "")));
    }
}
