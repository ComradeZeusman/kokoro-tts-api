use kokoro_en::{KokoroTts, Voice};
use std::error::Error;
use std::fs::File;
use std::io::Write;

fn save_wav(path: &str, samples: &[f32]) -> Result<(), Box<dyn Error>> {
    let mut file = File::create(path)?;

    let sample_rate = 24000u32;
    let channels = 1u16;
    let bits_per_sample = 16u16;

    let data_size = (samples.len() * 2) as u32;

    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_size).to_le_bytes())?;
    file.write_all(b"WAVE")?;

    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&(sample_rate * 2).to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&bits_per_sample.to_le_bytes())?;

    file.write_all(b"data")?;
    file.write_all(&data_size.to_le_bytes())?;

    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        file.write_all(&value.to_le_bytes())?;
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .build()?;

    runtime.block_on(async {
        println!("Loading Kokoro model...");

        let tts = KokoroTts::new(
            "models/model_quantized.onnx",
            "voices"
        ).await?;

        println!("Model loaded!");

        let text = "Hello! Welcome to my offline text to speech server.";

        println!("Generating speech...");

        let (audio, duration) = tts
            .synth(text, Voice::new("af_heart"))
            .await?;

        println!("Synthesis took: {:?}", duration);
        println!("Generated {} samples", audio.len());

        save_wav("output.wav", &audio)?;

        println!("Audio saved to output.wav");

        Ok::<(), Box<dyn Error>>(())
    })?;

    Ok(())
}
