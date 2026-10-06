//! Every pane follows The Elm Architecture: one state, messages, an `update`
//! that alone changes the state and returns effects, a `view` that draws it.
//! A network call is never awaited in the loop: it is an effect run in a
//! tokio task whose result comes back as a message. `update` is therefore
//! testable without a terminal or a database.

pub mod console;
pub mod ddl;
pub mod editor;
pub mod grid;
pub mod highlight;
pub mod inspector;
pub mod keys;
pub mod picker;
pub mod prompt;
pub mod quickdoc;
pub mod table;
pub mod theme;
pub mod tree;
pub mod widgets;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream, KeyEvent,
    KeyEventKind, MouseEvent,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::Frame;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum Input<M> {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize,
    Tick,
    Msg(M),
}

pub trait Program {
    type Msg: Send + 'static;
    type Effect;

    fn init(&mut self) -> Vec<Self::Effect> {
        Vec::new()
    }
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Self::Effect>;
    fn view(&mut self, frame: &mut Frame);
    fn quit(&self) -> bool;
}

/// Runs effects: spawns tasks that send messages back.
pub trait Perform<P: Program> {
    fn perform(&mut self, effect: P::Effect, tx: &mpsc::UnboundedSender<P::Msg>);
}

pub type Sender<M> = mpsc::UnboundedSender<M>;

pub const TICK: Duration = Duration::from_millis(250);

/// Runs a pane until it quits and returns its final state.
pub async fn run<P: Program, E: Perform<P>>(mut program: P, mut executor: E) -> anyhow::Result<P> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let (tx, mut rx) = mpsc::unbounded_channel();
    for effect in program.init() {
        executor.perform(effect, &tx);
    }
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    // Herdr closing the pane hangs the terminal up: exit cleanly so tunnel
    // leases and connections are released.
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = loop {
        if let Err(e) = terminal.draw(|frame| program.view(frame)) {
            break Err(e.into());
        }
        let input = tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => Input::Key(key),
                Some(Ok(Event::Mouse(mouse))) => Input::Mouse(mouse),
                Some(Ok(Event::Paste(text))) => Input::Paste(text),
                Some(Ok(Event::Resize(..))) => Input::Resize,
                Some(Ok(_)) => continue,
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
            Some(msg) = rx.recv() => Input::Msg(msg),
            _ = ticker.tick() => Input::Tick,
            _ = hangup.recv() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
        };
        for effect in program.update(input) {
            executor.perform(effect, &tx);
        }
        if program.quit() {
            break Ok(());
        }
    };
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    drop(executor);
    result.map(|()| program)
}
