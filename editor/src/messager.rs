use std::num::NonZeroU32;

use crate::gpu::Gpu;

#[derive(Clone, Copy, Default, Debug)]
pub struct Message {
    // High 20 bits: blob offset, low 12 bits: len
    pub blob_offset_and_len: u32,

    pub  pushed_at: u32,
    pub started_at: Option<NonZeroU32>,

    pub width: f32
}

impl Message {
    const LEN_BITS: u32 = 12;
    const LEN_MASK: u32 = (1 << Self::LEN_BITS) - 1; // 0xFFF

    #[inline]
    pub fn new(blob_offset: u32, len: u16, pushed_at: u32, width: f32) -> Self {
        debug_assert!(blob_offset < (1 << 20),        "blob_offset exceeds 20 bits");
        debug_assert!((len as u32) <= Self::LEN_MASK, "len exceeds 12 bits");

        Self {
            blob_offset_and_len: (blob_offset << Self::LEN_BITS) | (len as u32 & Self::LEN_MASK),
            pushed_at,
            width,
            ..Default::default()
        }
    }

    #[inline]
    pub fn blob_offset(&self) -> u32 {
        self.blob_offset_and_len >> Self::LEN_BITS
    }

    #[inline]
    pub fn len(&self) -> u16 {
        (self.blob_offset_and_len & Self::LEN_MASK) as u16
    }
}

pub const MAX_MESSAGE_COUNT: usize = 32;

pub const MESSAGE_DURATION_IN_MILLISECONDS: u32 = 2900;

pub const MESSAGER_FONT_SIZE: f32 = 12.0;

#[derive(Debug)]
pub struct Messager {
    pub blob:    String,

    pub column_width: f32,  // Only grows while messages visible, resets when empty

    pub tick:    u32,

    pub archive: Vec<Message>,

    pub head:    u32,
    pub tail:    u32,
    pub count:   u32,
    pub entries: [Message; MAX_MESSAGE_COUNT],
}

impl Messager {
    pub fn new() -> Self {
        Self {
            blob:    String::new(),
            entries: [Message::default(); MAX_MESSAGE_COUNT],
            head:    0,
            tail:    0,
            count:   0,
            tick:    1,
            column_width: 0.0,
            archive: Vec::new(),
        }
    }

    pub fn tick(&mut self, dt: f32) {
        self.tick += (dt * 1000.0) as u32;
    }

    pub fn push(&mut self, text: &str, gpu: &mut Gpu) {
        if self.count == MAX_MESSAGE_COUNT as u32 {
            //
            // Evict head to archive before overwriting
            //
            self.archive.push(self.entries[self.head as usize]);
            self.head = (self.head + 1) % MAX_MESSAGE_COUNT as u32;
            self.count -= 1;
        }

        let offset = self.blob.len();
        self.blob.push_str(text);

        let width = gpu.measure_message(text);

        let message = Message::new(offset as u32, text.len() as u16, self.tick, width);
        self.entries[self.tail as usize] = message;
        self.tail = (self.tail + 1) % MAX_MESSAGE_COUNT as u32;
        self.count += 1;

        self.column_width = self.column_width.max(width);
    }

    pub fn evict_expired(&mut self, ttl: u32) {
        while self.count > 0 {
            let message = self.entries[self.head as usize];
            if self.tick.wrapping_sub(message.pushed_at) < ttl {
                break;
            }
            self.archive.push(message);
            self.head = (self.head + 1) % MAX_MESSAGE_COUNT as u32;
            self.count -= 1;
        }

        if self.count == 0 {
            self.column_width = 0.0;
        }
    }

    pub fn get(&self, msg: &Message) -> &str {
        let offset = msg.blob_offset() as usize;
        let len    = msg.len()         as usize;
        &self.blob[offset..offset + len]
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Message)> {
        (0..self.count as usize).filter_map(|index| {
            let index = (self.head as usize + index) % MAX_MESSAGE_COUNT;
            let message = self.entries.get(index)?;
            Some((self.get(message), message))
        })
    }
}
