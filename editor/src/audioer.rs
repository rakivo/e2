use std::time::Instant;
use std::io::Cursor;
use std::thread;
use crossbeam_channel::Sender;
use rodio::{Decoder, DeviceSinkBuilder, Player};
use rustc_hash::FxHashMap;

const LISTER_ITEM_HOVER_SOUND: &[u8] = include_bytes!("../assets/lister-mouse-hover.wav");
const           STARTUP_SOUND: &[u8] = include_bytes!("../assets/startup.wav");
const    LISTER_CONFIRM_SOUND: &[u8] = include_bytes!("../assets/lister-confirm.wav");

pub const LISTER_ITEM_HOVER: SoundId = SoundId(0);
pub const STARTUP:           SoundId = SoundId(1);
pub const LISTER_CONFIRM:    SoundId = SoundId(2);

#[derive(Hash, Ord, Eq, PartialEq, PartialOrd, Clone, Copy)]
pub struct SoundId(u16);
cranelift_entity::entity_impl!(SoundId, "SoundId", id, SoundId(id as u16), id.0 as u32);

pub enum AudioCommand {
    Play { id: SoundId, bytes: &'static [u8], volume: f32 },
    Stop { id: SoundId },
    SetVolume { id: SoundId, volume: f32 },
}

#[derive(Clone)]
pub struct Audioer {
    tx: Sender<AudioCommand>,
}

impl Audioer {
    pub fn spawn() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();

        thread::spawn(move || {
            let t = Instant::now();

            let mut handle = match DeviceSinkBuilder::open_default_sink() {
                Ok(ok) => ok,
                Err(e) => { eprintln!("Error: Couldn't open default sink: {e}"); return }
            };
            handle.log_on_drop(false);

            eprintln!("[Initialized audio in {}ms]", t.elapsed().as_millis() as f32);

            let mixer = handle.mixer();
            let mut players = FxHashMap::default();

            while let Ok(cmd) = rx.recv() {
                players.retain(|_, p: &mut Player| !p.empty());

                match cmd {
                    AudioCommand::Play { id, bytes, volume } => {
                        let source = match Decoder::new(Cursor::new(bytes)) {
                            Ok(ok) => ok,
                            Err(e) => { eprintln!("Error: Couldn't decode {id}: {e}"); return }
                        };

                        let player = Player::connect_new(mixer);
                        player.set_volume(volume);

                        player.append(source);
                        players.insert(id, player);
                    }

                    AudioCommand::Stop { id } => {
                        if let Some(p) = players.remove(&id) {
                            p.stop();
                        }
                    }

                    AudioCommand::SetVolume { id, volume } => {
                        if let Some(p) = players.get(&id) {
                            p.set_volume(volume);
                        }
                    }
                }
            }
        });

        Self { tx }
    }

    pub fn play(&self, id: SoundId, bytes: &'static [u8], volume: f32) {
        self.tx.send(AudioCommand::Play { id, bytes, volume }).ok();
    }

    pub fn stop(&self, id: SoundId) {
        self.tx.send(AudioCommand::Stop { id }).ok();
    }

    pub fn set_volume(&self, id: SoundId, volume: f32) {
        self.tx.send(AudioCommand::SetVolume { id, volume }).ok();
    }

    pub fn play_lister_item_hover_sound(&self) {
        self.stop(LISTER_ITEM_HOVER);
        self.play(LISTER_ITEM_HOVER, LISTER_ITEM_HOVER_SOUND, 0.009);
    }

    pub fn play_startup_sound(&self) {
        self.stop(STARTUP);
        self.play(STARTUP, STARTUP_SOUND, 0.4);
    }

    pub fn play_lister_confirm(&self) {
        self.stop(LISTER_CONFIRM);
        self.play(LISTER_CONFIRM, LISTER_CONFIRM_SOUND, 0.4);
    }
}
