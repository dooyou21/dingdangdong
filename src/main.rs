use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, Stream};
use device_query::{DeviceQuery, DeviceState, Keycode};
use ringbuf::traits::Split;
use ringbuf::{traits::*, HeapRb};
use std::collections::HashMap;
use std::thread;
use std::time::Duration;

// 오실리에이터 유형
#[derive(Debug, Clone, Copy)]
enum Oscillator {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

// 노트 정보를 저장할 구조체
#[derive(Debug, Clone)]
struct Note {
    frequency: f32,
    is_playing: bool,
    oscillator: Oscillator,
    amplitude: f32,
}

impl Note {
    fn new(frequency: f32, oscillator: Oscillator) -> Self {
        Self {
            frequency,
            is_playing: false,
            oscillator,
            amplitude: 0.0,
        }
    }

    // 주파수에 따른 샘플 생성
    fn generate_sample(&mut self, phase: &mut f32, sample_rate: f32) -> f32 {
        if !self.is_playing {
            return 0.0;
        }

        // 위상 증가
        *phase += self.frequency / sample_rate;
        if *phase >= 1.0 {
            *phase -= 1.0;
        }

        // 오실리에이터 유형에 따른 파형 생성
        let sample = match self.oscillator {
            Oscillator::Sine => (2.0 * std::f32::consts::PI * *phase).sin(),
            Oscillator::Square => {
                if *phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Oscillator::Sawtooth => 2.0 * *phase - 1.0,
            Oscillator::Triangle => {
                if *phase < 0.5 {
                    4.0 * *phase - 1.0
                } else {
                    3.0 - 4.0 * *phase
                }
            }
        };
        sample * self.amplitude
    }
}

// 키보드 키와 주파수 매핑
fn create_key_frequency_map() -> HashMap<Keycode, f32> {
    let mut map = HashMap::new();

    map.insert(Keycode::Z, 261.63); // C4
    map.insert(Keycode::S, 277.18); // C#4
    map.insert(Keycode::X, 293.66); // D4
    map.insert(Keycode::D, 311.13); // D#4
    map.insert(Keycode::C, 329.63); // E4
    map.insert(Keycode::V, 349.23); // F4
    map.insert(Keycode::G, 369.99); // F#4
    map.insert(Keycode::B, 392.99); // G4
    map.insert(Keycode::H, 415.30); // G#4
    map.insert(Keycode::N, 440.00); // A4
    map.insert(Keycode::J, 466.16); // A#4
    map.insert(Keycode::M, 493.88); // B4
    map.insert(Keycode::Comma, 523.25); // C5

    map
}

// ADSR 엔벨로프
struct Envelope {
    attack_time: f32,
    decay_time: f32,
    sustain_level: f32,
    release_time: f32,
    current_level: f32,
    phase: EnvelopePhase,
    sample_rate: f32,
    samples_processed: usize,
}

enum EnvelopePhase {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

impl Envelope {
    fn new(
        attack_time: f32,
        decay_time: f32,
        sustain_level: f32,
        release_time: f32,
        sample_rate: f32,
    ) -> Self {
        Self {
            attack_time,
            decay_time,
            sustain_level,
            release_time,
            current_level: 0.0,
            phase: EnvelopePhase::Idle,
            sample_rate,
            samples_processed: 0,
        }
    }

    fn trigger(&mut self) {
        self.phase = EnvelopePhase::Attack;
        self.samples_processed = 0;
    }

    fn release(&mut self) {
        self.phase = EnvelopePhase::Release;
        self.samples_processed = 0;
    }

    fn process(&mut self) -> f32 {
        match self.phase {
            EnvelopePhase::Idle => 0.0,
            EnvelopePhase::Attack => {
                let attack_samples = (self.attack_time * self.sample_rate) as usize;
                if attack_samples == 0 {
                    self.current_level = 1.0;
                    self.phase = EnvelopePhase::Decay;
                    self.samples_processed = 0;
                } else {
                    self.current_level = self.samples_processed as f32 / attack_samples as f32;
                    if self.samples_processed >= attack_samples {
                        self.phase = EnvelopePhase::Decay;
                        self.samples_processed = 0;
                    }
                }
                self.samples_processed += 1;
                self.current_level
            }
            EnvelopePhase::Decay => {
                let decay_samples = (self.decay_time * self.sample_rate) as usize;
                if decay_samples == 0 {
                    self.current_level = self.sustain_level;
                    self.phase = EnvelopePhase::Sustain;
                } else {
                    self.current_level = 1.0
                        - (1.0 - self.sustain_level)
                            * (self.samples_processed as f32 / decay_samples as f32);
                    if self.samples_processed >= decay_samples {
                        self.phase = EnvelopePhase::Sustain;
                    }
                }
                self.samples_processed += 1;
                self.current_level
            }
            EnvelopePhase::Sustain => self.sustain_level,
            EnvelopePhase::Release => {
                let release_samples = (self.release_time * self.sample_rate) as usize;
                if release_samples == 0 {
                    self.current_level = 0.0;
                    self.phase = EnvelopePhase::Idle;
                } else {
                    self.current_level = self.sustain_level
                        * (1.0 - self.samples_processed as f32 / release_samples as f32);
                    if self.samples_processed >= release_samples {
                        self.current_level = 0.0;
                        self.phase = EnvelopePhase::Idle;
                    }
                }
                self.samples_processed += 1;
                self.current_level
            }
        }
    }
}

// 오디오 스트림 생성
fn create_audio_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut consumer: impl Consumer<Item = (Keycode, bool)> + Send + 'static,
) -> Result<Stream> {
    let sample_rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;

    // 노트 맵 생성
    let key_frequency_map = create_key_frequency_map();

    // 활성화된 노트 추적
    let mut notes: HashMap<Keycode, (Note, f32, Envelope)> = HashMap::new();

    // 오실레이터 선택 (기본적으로 사인파)
    let oscillator_type = Oscillator::Sine;

    let err_fn = |err| eprintln!("Audio Stream Error: {}", err);

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // 키보드 이벤트 처리
                while let Some((key, pressed)) = consumer.try_pop() {
                    if let Some(&frequency) = key_frequency_map.get(&key) {
                        if pressed {
                            if !notes.contains_key(&key) {
                                let mut note = Note::new(frequency, oscillator_type);
                                note.is_playing = true;
                                let envelope = Envelope::new(0.01, 0.1, 0.7, 0.2, sample_rate);
                                notes.insert(key, (note, 0.0, envelope));
                            }

                            if let Some((note, _, envelope)) = notes.get_mut(&key) {
                                note.is_playing = true;
                                envelope.trigger();
                            }
                        } else {
                            if let Some((note, _, envelope)) = notes.get_mut(&key) {
                                envelope.release();
                            }
                        }
                    }
                }

                // 오디오 샘플 생성
                for frame in data.chunks_mut(channels) {
                    let mut mix = 0.0;

                    // 모든 활성화된 노트에 대해 샘플 생성
                    for (_, (note, phase, envelope)) in notes.iter_mut() {
                        let env_value = envelope.process();
                        let mut note_clone = note.clone();
                        note_clone.amplitude = env_value * 0.2;
                        let sample = note_clone.generate_sample(phase, sample_rate);
                        mix += sample;
                    }

                    // 채널 수에 따라 모든 채널에 같은 값 할당
                    for channel in frame.iter_mut() {
                        *channel = Sample::to_sample(mix);
                    }

                    // 더이상 사용하지 않는 노트 제거
                    notes.retain(|_, (_, _, envelope)| match envelope.phase {
                        EnvelopePhase::Idle => false,
                        _ => true,
                    });
                }
            },
            err_fn,
            None,
        )?,
        // 다른 포맷 필요한경우 여기에 추가
        _ => return Err(anyhow::anyhow!("Cannot handle this format")),
    };

    Ok(stream)
}

fn main() -> Result<()> {
    // 오디오 호스트 및 디바이스 설정
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("Cannot find output device");
    let config = device
        .default_output_config()
        .expect("Cannot find output config");

    println!("Default output device: {:?}", device.name());
    println!("Default output config: {:?}", config);

    // 키보드 이벤트 처리를 위한 링 버퍼
    let ring_buffer = HeapRb::<(Keycode, bool)>::new(1024);
    let (mut producer, consumer) = ring_buffer.split();

    // 오디오 스트림 생성 및 시작
    let stream = create_audio_stream(&device, &config, consumer)?;
    stream.play()?;

    // 키보드 상태 모니터링
    let device_state = DeviceState::new();
    let mut previous_keys = Vec::new();

    println!("Minimal Toy Synthesizer!");
    println!("Use Z-M keys to play");
    println!("Press Esc to exit");

    loop {
        let keys = device_state.get_keys();

        for key in &keys {
            if !previous_keys.contains(key) {
                producer.try_push((*key, true)).unwrap();
            }
        }

        for key in &previous_keys {
            if !keys.contains(key) {
                producer.try_push((*key, false)).unwrap()
            }
        }

        if keys.contains(&Keycode::Escape) {
            break;
        }

        previous_keys = keys;
        thread::sleep(Duration::from_millis(5));
    }

    Ok(())
}
