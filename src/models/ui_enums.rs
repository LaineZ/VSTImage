use crate::rack::instance::InputChannelType;

#[derive(Debug)]
pub enum Action {
    OpenEditor(usize),
    Remove(usize),
    Bypass(usize),
    ChangeInputChannel(usize, InputChannelType),
    ChangeOutputChannel(usize, usize),
    ChangeWet(usize, f32),
    ChangeSampleRate(usize, f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tool {
    Brush,
    Hand,
}

#[derive(Clone, Copy, Debug)]
pub enum ModalWindows {
    Exit,
    ExitNew,
    About,
    None,
}

pub enum DialogVariant {
    Yes,
    No,
    Cancel,
    None,
}
