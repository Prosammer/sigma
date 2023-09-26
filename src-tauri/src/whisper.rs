extern crate anyhow;
extern crate cpal;
extern crate ringbuf;

use std::mem::MaybeUninit;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{Consumer, SharedRb};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperState};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::sleep;
use std::time::Duration;
use anyhow::Error;
use async_openai::types::{ChatCompletionRequestMessage, Role};
use cpal::{Stream, StreamConfig};
use futures::executor::block_on;
use tauri::AppHandle;
use tauri::async_runtime::{Receiver, Sender};
use once_cell::sync::OnceCell;
use std::path::Path;
use crate::audio_utils;
use crate::audio_utils::{convert_stereo_to_mono_audio, make_audio_louder};
use crate::gpt::create_chat_completion_request_msg;
use crate::stores::get_from_store;

pub const LATENCY_MS: f32 = 7000.0;
pub static WHISPER_CONTEXT: OnceCell<WhisperContext> = OnceCell::new();

pub async fn init_whisper_context() {
    let whisper_path_str = "src/ggml-base.en.bin";
    let whisper_path = Path::new(whisper_path_str);
    if !whisper_path.exists() && !whisper_path.is_file() {
        panic!("expected a whisper directory")
    }
    let ctx = WhisperContext::new(whisper_path_str).expect("Failed to open model");
    WHISPER_CONTEXT.set(ctx).expect("Failed to set WhisperContext");
}


pub fn send_system_audio_to_channel(audio_tx: Sender<Vec<f32>>, mut resume_channel_rx: Receiver<bool>, should_quit: Arc<AtomicBool>) {
    let (config, mut consumer, input_stream) = setup_audio().expect("Failed to setup audio");

    // Ensure the initial speech is finished before starting the input stream
    input_stream.play().expect("Failed to play input stream");
    // Remove the initial samples
    consumer.clear();
    sleep(Duration::from_millis(2000));

    loop {
        let samples: Vec<f32> = consumer.iter().map(|x| *x).collect();
        // TODO: Instead of removing every second sample, just set the input data fn to only push every second sample
        let samples = convert_stereo_to_mono_audio(samples).unwrap();
        let mut samples = make_audio_louder(&samples, 2.0);

        let sampling_freq = config.sample_rate.0 as f32 / 2.0; // TODO: Divide by 2 because of stereo to mono

        if audio_utils::vad_simple(&mut samples, sampling_freq as usize, 1000) {
            // the last 1000ms of audio was silent and there was talking before it
            println!("Speech detected! Pausing input stream...");
            input_stream.pause().expect("Failed to pause input stream");
            block_on(async { audio_tx.send(samples).await.expect("Failed to send audio to channel") });
            consumer.clear();

            // Wait for the resume_stream message
            loop {
                if let Some(resume_stream) = block_on(resume_channel_rx.recv()) {
                    if resume_stream == true {
                        println!("Resuming input stream...");
                        input_stream.play().expect("Failed to play input stream");
                        break;
                    }
                }
                sleep(Duration::from_millis(200));
            }
        } else {
            // Else, there is just silence. The samples should be deleted
            println!("Silence Detected!");
            sleep(Duration::from_secs(1));
            // TODO: Clear some of the buffer to avoid latency issues - use popiter
            // if consumer.len() > latency_samples / 2 {
            //     println!("Clearing half of the buffer");
            //     consumer.skip(latency_samples / 2);
            // }
        }
        if should_quit.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
    }
    // Update messages to be the last message on the messages_update_channel_rx
    // let session_messages = messages_update_channel_rx.latest().clone();
    // println!("Messages: {:?}", session_messages);
}

pub async fn messages_setup(handle: AppHandle) -> Vec<ChatCompletionRequestMessage> {
    let system_message_content = "You are an AI personal routine trainer. You greet the user in the morning, then go through the user-provided morning routine checklist and ensure that the user completes each task on the list in order. Make sure to keep your tone positive, but it is vital that the user completes each task - do not allow them to 'skip' tasks. The user uses speech-to-text to communicate, so some of their messages may be incorrect - if some text seems out of place, please ignore it. If the users sentence makes no sense in the context, tell them you don't understand and ask them to repeat themselves. If you receive any text like [SILENCE] or [MUSIC] please respond with - I didn't catch that. The following message is the prompt the user provided - their morning checklist. Call the leave_conversation function when the user has completed their morning routine, or whenever the AI would normally say goodbye";
    let system_message = create_chat_completion_request_msg(system_message_content.to_string(), Role::System);

    let user_prompt_content = get_from_store(handle, "userPrompt").unwrap_or("".to_string());
    let user_prompt_message = create_chat_completion_request_msg(user_prompt_content, Role::System);

    return vec![system_message, user_prompt_message]
}

fn setup_audio() -> Result<(StreamConfig, Consumer<f32, Arc<SharedRb<f32, Vec<MaybeUninit<f32>>>>>, Stream), Error> {
    let host = cpal::default_host();
    let input_device = host
        .default_input_device()
        .expect("failed to get default input device");
    println!("Using default input device: \"{}\"", input_device.name()?);
    let config = input_device
        .default_input_config()
        .expect("Failed to get default input config").config();
    println!("Default input config: {:?}", config);

    // Top level variables
    let latency_frames = (LATENCY_MS / 1_000.0) * config.sample_rate.0 as f32;
    let latency_samples = latency_frames as usize * config.channels as usize;
    println!("{}", latency_samples);

    // The buffer to share samples
    let ring = SharedRb::new(latency_samples * 2);
    let (mut producer, consumer) = ring.split();

    // Setup microphone callback
    let input_data_fn = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        let mut output_fell_behind = false;
        for &sample in data {
            if producer.push(sample).is_err() {
                output_fell_behind = true;
            }
        }
        if output_fell_behind {
            eprintln!("output stream fell behind: try increasing latency");
        }
    };

    // Build streams.
    println!(
        "Attempting to build both streams with f32 samples and `{:?}`.",
        config
    );
    println!("Setup input stream");
    let input_stream = input_device.build_input_stream(&config, input_data_fn, err_fn, None)?;
    Ok((config, consumer, input_stream))
}

pub fn speech_to_text(samples: &Vec<f32>, state: &mut WhisperState) -> String {
    let mut params = FullParams::new(SamplingStrategy::default());
    params.set_print_progress(false);
    params.set_print_special(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_language(Some("en"));
    params.set_token_timestamps(true);
    params.set_duration_ms(LATENCY_MS as i32);
    params.set_no_context(true);
    params.set_n_threads(8);

    //params.set_no_speech_thold(0.3);
    //params.set_split_on_word(true);

    state
        .full(params, &*samples)
        .expect("failed to convert samples");

    let num_tokens = state.full_n_tokens(0).expect("Error");
    let words = (1..num_tokens - 1)
        .map(|i| state.full_get_token_text(0, i).expect("Error"))
        .collect::<String>();

    words
}

fn err_fn(err: cpal::StreamError) {
    eprintln!("an error occurred on stream: {}", err);
}