#[derive(Clone)]
pub struct Audioer;

impl Audioer {
    pub fn spawn() -> Self { Self }
    pub fn play_lister_item_hover_sound(&self) {}
    pub fn play_startup_sound(&self) {}
    pub fn play_lister_confirm(&self) {}
}
