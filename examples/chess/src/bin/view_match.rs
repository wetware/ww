//! PGN replay viewer for the ChessEngine example.
//!
//! Usage: cargo run -p chess --bin view_match -- game.pgn
//!
//! Replays moves from a PGN file with a 1.5 second delay between moves.
//! Controls: Space = pause/resume, Right = step forward, Left = step back, R = restart

use macroquad::prelude::*;
use shakmaty::fen::Fen;
use shakmaty::san::San;
use shakmaty::uci::UciMove;
use shakmaty::{Chess, EnPassantMode, Position};

const WINDOW_WIDTH: i32 = 860;
const WINDOW_HEIGHT: i32 = 720;
const BOARD_PADDING: f32 = 28.0;
const LIGHT_SQUARE: Color = Color::from_hex(0xf0d9b5);
const DARK_SQUARE: Color = Color::from_hex(0xb58863);
const LAST_MOVE: Color = Color::new(0.28, 0.56, 0.87, 0.45);

const MOVE_DELAY: f64 = 1.5;
const END_DELAY: f64 = 3.0;

fn window_conf() -> Conf {
    Conf {
        window_title: "Wetware Chess — PGN Replay".into(),
        window_width: WINDOW_WIDTH,
        window_height: WINDOW_HEIGHT,
        window_resizable: true,
        ..Default::default()
    }
}

/// Parse PGN movetext into a list of SAN strings.
fn parse_pgn_moves(pgn: &str) -> Vec<String> {
    let movetext: String = pgn
        .lines()
        .filter_map(normalize_pgn_line)
        .filter(|line| !line.starts_with('[') && !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    movetext
        .split_whitespace()
        .filter(|token| {
            !token.contains('.')
                && *token != "1-0"
                && *token != "0-1"
                && *token != "1/2-1/2"
                && *token != "*"
        })
        .map(|s| s.to_string())
        .collect()
}

/// Recover the guest message from a PGN captured through the host tracer.
/// New matches are written without this wrapper, but accepting it keeps old
/// captures viewable as well.
fn normalize_pgn_line(line: &str) -> Option<String> {
    let plain = strip_ansi(line);
    if plain.contains("---PGN_START---") || plain.contains("---PGN_END---") {
        return None;
    }

    let message = plain
        .rsplit_once("ww::launcher:")
        .map(|(_, message)| message)
        .or_else(|| plain.rsplit_once("[INFO]").map(|(_, message)| message))
        .or_else(|| plain.strip_prefix("INFO "))
        .unwrap_or(&plain);
    Some(message.trim_start().to_string())
}

fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'\x1b' && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && !(b'@'..=b'~').contains(&bytes[index]) {
                index += 1;
            }
            index += usize::from(index < bytes.len());
        } else {
            plain.push(bytes[index] as char);
            index += 1;
        }
    }

    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_traced_pgn_movetext() {
        let traced = "\x1b[2m2026-09-09T20:53:37Z\x1b[0m \x1b[2mww::launcher\x1b[0m\x1b[2m:\x1b[0m [Event \"Wetware Chess\"]\n\
                      \x1b[2m2026-09-09T20:53:37Z\x1b[0m \x1b[2mww::launcher\x1b[0m\x1b[2m:\x1b[0m 1. e4 e5 1/2-1/2\n";
        assert_eq!(parse_pgn_moves(traced), vec!["e4", "e5"]);
    }

    #[test]
    fn ignores_pgn_boundary_log_records() {
        let traced = "INFO ---PGN_START---\n\
                      INFO 1. e4 e5 1/2-1/2\n\
                      INFO ---PGN_END---\n";
        assert_eq!(parse_pgn_moves(traced), vec!["e4", "e5"]);
    }

    #[test]
    fn loads_the_captured_game() {
        let moves = parse_pgn_moves(include_str!("../../../../game.pgn"));
        assert!(moves.len() > 1);
        assert!(build_positions(&moves).is_ok());
    }
}

/// Build all positions from the starting position through each SAN move.
/// Returns (positions, last_moves) where positions[0] is the start and
/// last_moves[i] is the (from, to) for the move that produced positions[i+1].
fn build_positions(
    san_moves: &[String],
) -> Result<(Vec<Chess>, Vec<((usize, usize), (usize, usize))>), String> {
    let mut positions = vec![Chess::default()];
    let mut last_moves = Vec::new();

    let mut pos = Chess::default();
    for (index, san_str) in san_moves.iter().enumerate() {
        let san: San = san_str
            .parse()
            .map_err(|e| format!("move {}: failed to parse SAN '{san_str}': {e}", index + 1))?;
        let m = san
            .to_move(&pos)
            .map_err(|e| format!("move {}: illegal SAN '{san_str}': {e}", index + 1))?;

        // Extract from/to squares for the highlight
        let uci = UciMove::from_standard(&m).to_string();
        let from = parse_square(&uci[0..2])
            .ok_or_else(|| format!("move {}: invalid source square in '{uci}'", index + 1))?;
        let to = parse_square(&uci[2..4])
            .ok_or_else(|| format!("move {}: invalid destination square in '{uci}'", index + 1))?;
        last_moves.push((from, to));

        pos.play_unchecked(&m);
        positions.push(pos.clone());
    }

    Ok((positions, last_moves))
}

fn position_fen(pos: &Chess) -> String {
    Fen::from_position(pos.clone(), EnPassantMode::Legal).to_string()
}

struct ReplayState {
    san_moves: Vec<String>,
    positions: Vec<Chess>,
    last_moves: Vec<((usize, usize), (usize, usize))>,
    move_index: usize, // 0 = start position, 1 = after first move, etc.
    paused: bool,
    last_advance_time: f64,
}

impl ReplayState {
    fn new(san_moves: Vec<String>) -> Result<Self, String> {
        let (positions, last_moves) = build_positions(&san_moves)?;
        Ok(Self {
            san_moves,
            positions,
            last_moves,
            move_index: 0,
            paused: false,
            last_advance_time: get_time(),
        })
    }

    fn restart(&mut self) {
        self.move_index = 0;
        self.last_advance_time = get_time();
    }

    fn at_end(&self) -> bool {
        self.move_index >= self.san_moves.len()
    }

    fn step_forward(&mut self) {
        if !self.at_end() {
            self.move_index += 1;
            self.last_advance_time = get_time();
        }
    }

    fn step_back(&mut self) {
        if self.move_index > 0 {
            self.move_index -= 1;
            self.last_advance_time = get_time();
        }
    }

    fn current_fen(&self) -> String {
        position_fen(&self.positions[self.move_index])
    }

    fn current_last_move(&self) -> Option<((usize, usize), (usize, usize))> {
        if self.move_index > 0 {
            Some(self.last_moves[self.move_index - 1])
        } else {
            None
        }
    }

    fn last_san(&self) -> Option<&str> {
        if self.move_index > 0 {
            Some(&self.san_moves[self.move_index - 1])
        } else {
            None
        }
    }

    /// Full move number (1-based) for the current move index.
    fn move_number(&self) -> Option<(u32, &'static str)> {
        if self.move_index == 0 {
            return None;
        }
        let idx = self.move_index - 1; // 0-based move index
        let full_move = (idx / 2 + 1) as u32;
        let side = if idx % 2 == 0 { "White" } else { "Black" };
        Some((full_move, side))
    }
}

/// Try to load a font that supports Unicode chess symbols.
async fn load_piece_font() -> Option<Font> {
    for path in [
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\seguisym.ttf",
    ] {
        if let Ok(font) = load_ttf_font(path).await {
            return Some(font);
        }
    }
    None
}

#[macroquad::main(window_conf)]
async fn main() {
    let piece_font = load_piece_font().await;

    let pgn_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: view_match <game.pgn>");
        std::process::exit(1);
    });

    let pgn = std::fs::read_to_string(&pgn_path).unwrap_or_else(|e| {
        eprintln!("Failed to read '{}': {}", pgn_path, e);
        std::process::exit(1);
    });

    let san_moves = parse_pgn_moves(&pgn);
    if san_moves.is_empty() {
        eprintln!("No moves found in PGN file.");
        std::process::exit(1);
    }

    let mut state = match ReplayState::new(san_moves) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("Failed to load PGN: {error}");
            return;
        }
    };

    loop {
        let layout = BoardLayout::for_screen();
        let now = get_time();

        // Keyboard controls
        if is_key_pressed(KeyCode::Space) {
            state.paused = !state.paused;
            state.last_advance_time = now;
        }
        if is_key_pressed(KeyCode::Right) {
            state.step_forward();
        }
        if is_key_pressed(KeyCode::Left) {
            state.step_back();
        }
        if is_key_pressed(KeyCode::R) {
            state.restart();
        }

        // Auto-advance
        if !state.paused {
            if state.at_end() {
                // Wait END_DELAY then loop
                if now - state.last_advance_time >= END_DELAY {
                    state.restart();
                }
            } else if now - state.last_advance_time >= MOVE_DELAY {
                state.step_forward();
            }
        }

        let fen = state.current_fen();
        let last_move = state.current_last_move();

        // Build status message
        let message = if state.at_end() {
            format!(
                "Game over. {} moves played. Restarting...",
                state.san_moves.len()
            )
        } else if let Some((num, side)) = state.move_number() {
            let san = state.last_san().unwrap();
            format!("Move {num}. {side}: {san}")
        } else {
            "Starting position".to_string()
        };

        draw_board(
            layout,
            &fen,
            last_move,
            &message,
            &state,
            piece_font.as_ref(),
        );
        next_frame().await;
    }
}

// ────────────────── Drawing infrastructure (preserved) ──────────────────

#[derive(Clone, Copy)]
struct BoardLayout {
    left: f32,
    top: f32,
    square: f32,
}

impl BoardLayout {
    fn for_screen() -> Self {
        let available_height = screen_height() - BOARD_PADDING * 2.0;
        let available_width = screen_width() - 250.0;
        let board_size = available_height.min(available_width).max(160.0);
        Self {
            left: BOARD_PADDING,
            top: (screen_height() - board_size) / 2.0,
            square: board_size / 8.0,
        }
    }

    fn rect(self, square: (usize, usize)) -> Rect {
        let (file, rank) = square;
        Rect::new(
            self.left + file as f32 * self.square,
            self.top + (7 - rank) as f32 * self.square,
            self.square,
            self.square,
        )
    }
}

fn draw_board(
    layout: BoardLayout,
    fen: &str,
    last_move: Option<((usize, usize), (usize, usize))>,
    message: &str,
    state: &ReplayState,
    piece_font: Option<&Font>,
) {
    clear_background(Color::from_hex(0x1b1d24));

    for rank in 0..8 {
        for file in 0..8 {
            let square = (file, rank);
            let rect = layout.rect(square);
            let color = if (file + rank) % 2 == 0 {
                LIGHT_SQUARE
            } else {
                DARK_SQUARE
            };
            draw_rectangle(rect.x, rect.y, rect.w, rect.h, color);
            if last_move.is_some_and(|(from, to)| square == from || square == to) {
                draw_rectangle(rect.x, rect.y, rect.w, rect.h, LAST_MOVE);
            }
        }
    }

    for (square, piece) in fen_pieces(fen) {
        let rect = layout.rect(square);
        let glyph = piece_glyph(piece);
        if glyph.is_empty() {
            continue;
        }
        let font_size = (layout.square * 0.72) as u16;
        let text_size = measure_text(glyph, piece_font, font_size, 1.0);
        let x = rect.x + (rect.w - text_size.width) / 2.0;
        let y = rect.y + (rect.h + text_size.height) / 2.0 - layout.square * 0.06;
        let color = if piece.is_ascii_uppercase() {
            Color::from_hex(0xfff8e7)
        } else {
            Color::from_hex(0x25211e)
        };
        match piece_font {
            Some(font) => {
                draw_text_ex(
                    glyph,
                    x,
                    y,
                    TextParams {
                        font: Some(font),
                        font_size,
                        color,
                        ..Default::default()
                    },
                );
            }
            None => {
                draw_text(glyph, x, y, font_size as f32, color);
            }
        }
    }

    for file in 0..8 {
        let label = ((b'a' + file as u8) as char).to_string();
        let rect = layout.rect((file, 0));
        draw_text(
            &label,
            rect.x + 5.0,
            rect.y + rect.h - 7.0,
            18.0,
            Color::from_hex(0x3e342d),
        );
    }
    for rank in 0..8 {
        let label = (rank + 1).to_string();
        let rect = layout.rect((0, rank));
        draw_text(
            &label,
            rect.x + 5.0,
            rect.y + 19.0,
            18.0,
            Color::from_hex(0x3e342d),
        );
    }

    // Side panel
    let panel_x = layout.left + layout.square * 8.0 + 30.0;
    draw_text(
        "WETWARE CHESS",
        panel_x,
        layout.top + 42.0,
        25.0,
        Color::from_hex(0xf2f2f5),
    );
    draw_text(
        "PGN Replay",
        panel_x,
        layout.top + 70.0,
        18.0,
        Color::from_hex(0xaab0c0),
    );
    draw_wrapped_text(
        message,
        panel_x,
        layout.top + 120.0,
        screen_width() - panel_x - 25.0,
        22.0,
    );

    // Progress
    let progress = format!("{} / {} moves", state.move_index, state.san_moves.len());
    draw_text(
        &progress,
        panel_x,
        layout.top + 180.0,
        18.0,
        Color::from_hex(0xaab0c0),
    );

    let status = if state.paused {
        "|| Paused"
    } else {
        "> Playing"
    };
    draw_text(
        status,
        panel_x,
        layout.top + 210.0,
        18.0,
        Color::from_hex(0xaab0c0),
    );

    // Controls help
    draw_text(
        "Space: pause/resume",
        panel_x,
        layout.top + 270.0,
        17.0,
        Color::from_hex(0xaab0c0),
    );
    draw_text(
        "Left/Right: step back/forward",
        panel_x,
        layout.top + 298.0,
        17.0,
        Color::from_hex(0xaab0c0),
    );
    draw_text(
        "R: restart",
        panel_x,
        layout.top + 326.0,
        17.0,
        Color::from_hex(0xaab0c0),
    );

    // FEN at bottom
    draw_text(
        &format!("FEN: {fen}"),
        BOARD_PADDING,
        screen_height() - 16.0,
        14.0,
        Color::from_hex(0xaab0c0),
    );
}

fn draw_wrapped_text(text: &str, x: f32, y: f32, width: f32, font_size: f32) {
    let mut line = String::new();
    let mut baseline = y;
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.into()
        } else {
            format!("{line} {word}")
        };
        if measure_text(&candidate, None, font_size as u16, 1.0).width > width && !line.is_empty() {
            draw_text(&line, x, baseline, font_size, Color::from_hex(0xf2f2f5));
            line = word.into();
            baseline += font_size * 1.35;
        } else {
            line = candidate;
        }
    }
    draw_text(&line, x, baseline, font_size, Color::from_hex(0xf2f2f5));
}

fn parse_square(square: &str) -> Option<(usize, usize)> {
    let bytes = square.as_bytes();
    if bytes.len() != 2 || !(b'a'..=b'h').contains(&bytes[0]) || !(b'1'..=b'8').contains(&bytes[1])
    {
        return None;
    }
    Some(((bytes[0] - b'a') as usize, (bytes[1] - b'1') as usize))
}

fn fen_pieces(fen: &str) -> Vec<((usize, usize), char)> {
    let mut pieces = Vec::new();
    let placement = fen.split_whitespace().next().unwrap_or_default();
    for (rank_from_top, row) in placement.split('/').enumerate() {
        let mut file = 0usize;
        for symbol in row.chars() {
            if let Some(empty) = symbol.to_digit(10) {
                file += empty as usize;
            } else if file < 8 && rank_from_top < 8 {
                pieces.push(((file, 7 - rank_from_top), symbol));
                file += 1;
            }
        }
    }
    pieces
}

fn piece_glyph(piece: char) -> &'static str {
    match piece {
        'K' => "\u{2654}",
        'Q' => "\u{2655}",
        'R' => "\u{2656}",
        'B' => "\u{2657}",
        'N' => "\u{2658}",
        'P' => "\u{2659}",
        'k' => "\u{265A}",
        'q' => "\u{265B}",
        'r' => "\u{265C}",
        'b' => "\u{265D}",
        'n' => "\u{265E}",
        'p' => "\u{265F}",
        _ => "",
    }
}
