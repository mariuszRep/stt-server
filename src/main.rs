use std::{env, error::Error, path::Path};
use transcribe_cpp::{backend_available, Backend, Model, ModelOptions, RunOptions};

fn read_wav(path: &Path) -> Result<Vec<f32>, Box<dyn Error>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate != 16_000 {
        return Err("proof fixture must be 16 kHz mono WAV".into());
    }
    match spec.sample_format {
        hound::SampleFormat::Int if spec.bits_per_sample == 16 => Ok(reader
            .samples::<i16>()
            .map(|s| s.map(|v| f32::from(v) / 32768.0))
            .collect::<Result<_, _>>()?),
        hound::SampleFormat::Float if spec.bits_per_sample == 32 => {
            Ok(reader.samples::<f32>().collect::<Result<_, _>>()?)
        }
        _ => Err("proof fixture must be 16-bit PCM or 32-bit float".into()),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args().collect();
    if !(3..=4).contains(&args.len()) {
        eprintln!("usage: stt-server MODEL.gguf AUDIO.wav [--cpu]");
        return Err("expected model and audio".into());
    }
    let model_path = Path::new(&args[1]);
    let audio = read_wav(Path::new(&args[2]))?;
    let cpu_only = args.get(3).is_some_and(|arg| arg == "--cpu");
    let (model, fallback_reason) = if cpu_only {
        (
            Model::load_with(
                model_path,
                &ModelOptions {
                    backend: Backend::Cpu,
                    ..Default::default()
                },
            )?,
            None,
        )
    } else if backend_available(Backend::Vulkan) {
        match Model::load_with(
            model_path,
            &ModelOptions {
                backend: Backend::Vulkan,
                ..Default::default()
            },
        ) {
            Ok(model) => (model, None),
            Err(error) => (
                Model::load_with(
                    model_path,
                    &ModelOptions {
                        backend: Backend::Cpu,
                        ..Default::default()
                    },
                )?,
                Some(error.to_string()),
            ),
        }
    } else {
        (
            Model::load_with(
                model_path,
                &ModelOptions {
                    backend: Backend::Cpu,
                    ..Default::default()
                },
            )?,
            Some("Vulkan backend unavailable".to_owned()),
        )
    };
    println!("observed_backend={}", model.backend());
    if let Some(reason) = fallback_reason {
        println!("fallback_reason={reason}");
    }
    let mut session = model.session()?;
    let result = session.run(&audio, &RunOptions::default())?;
    println!("{}", result.text);
    Ok(())
}
