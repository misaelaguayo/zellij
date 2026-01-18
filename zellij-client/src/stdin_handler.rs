use crate::keyboard_parser::KittyKeyboardParser;
use crate::os_input_output::ClientOsApi;
use crate::stdin_ansi_parser::StdinAnsiParser;
use crate::InputInstruction;
use std::sync::{Arc, Mutex};
use termwiz::input::{InputEvent, InputParser, MouseButtons};
use zellij_utils::channels::SenderWithContext;

/// Filter out kitty graphics responses from stdin buffer
/// These responses come from the parent terminal emulator when zellij sends kitty graphics
/// Format: \x1b_Gi=<id>;OK\x1b\\ or \x1b_Gi=<id>;E<error>\x1b\\
fn filter_kitty_graphics_responses(buf: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(buf.len());
    let mut i = 0;

    while i < buf.len() {
        // Look for ESC _ G (kitty graphics APC start)
        if i + 2 < buf.len() && buf[i] == 0x1b && buf[i + 1] == b'_' && buf[i + 2] == b'G' {
            // Found potential kitty graphics sequence, find the end
            let start = i;
            i += 3; // Skip ESC _ G

            // Find the string terminator (ESC \)
            let mut found_end = false;
            while i + 1 < buf.len() {
                if buf[i] == 0x1b && buf[i + 1] == b'\\' {
                    // Found the end - check if this looks like a response (contains ;OK or ;E)
                    let sequence = &buf[start..i + 2];
                    let seq_str = String::from_utf8_lossy(sequence);
                    if seq_str.contains(";OK") || seq_str.contains(";ENOENT") ||
                       seq_str.contains(";EINVAL") || seq_str.contains(";EBADF") ||
                       (seq_str.contains(";E") && sequence.len() < 50) {
                        // This is a kitty graphics response, skip it entirely
                        log::debug!("Filtering kitty graphics response from stdin: {:?}", seq_str);
                        i += 2; // Skip ESC \
                        found_end = true;
                        break;
                    } else {
                        // Not a response, keep it
                        result.extend_from_slice(sequence);
                        i += 2;
                        found_end = true;
                        break;
                    }
                }
                i += 1;
            }

            if !found_end {
                // Incomplete sequence, keep it for now
                result.extend_from_slice(&buf[start..]);
                break;
            }
        } else {
            result.push(buf[i]);
            i += 1;
        }
    }

    result
}

fn send_done_parsing_after_query_timeout(
    send_input_instructions: SenderWithContext<InputInstruction>,
    query_duration: u64,
) {
    std::thread::spawn({
        move || {
            std::thread::sleep(std::time::Duration::from_millis(query_duration));
            send_input_instructions
                .send(InputInstruction::DoneParsing)
                .unwrap();
        }
    });
}

pub(crate) fn stdin_loop(
    mut os_input: Box<dyn ClientOsApi>,
    send_input_instructions: SenderWithContext<InputInstruction>,
    stdin_ansi_parser: Arc<Mutex<StdinAnsiParser>>,
    explicitly_disable_kitty_keyboard_protocol: bool,
) {
    let mut holding_mouse = false;
    let mut input_parser = InputParser::new();
    let mut current_buffer = vec![];
    {
        // on startup we send a query to the terminal emulator for stuff like the pixel size and colors
        // we get a response through STDIN, so it makes sense to do this here
        let mut stdin_ansi_parser = stdin_ansi_parser.lock().unwrap();
        match stdin_ansi_parser.read_cache() {
            Some(events) => {
                let _ =
                    send_input_instructions.send(InputInstruction::AnsiStdinInstructions(events));
                let _ = send_input_instructions
                    .send(InputInstruction::DoneParsing)
                    .unwrap();
            },
            None => {
                send_input_instructions
                    .send(InputInstruction::StartedParsing)
                    .unwrap();
                let terminal_emulator_query_string =
                    stdin_ansi_parser.terminal_emulator_query_string();
                let _ = os_input
                    .get_stdout_writer()
                    .write(terminal_emulator_query_string.as_bytes())
                    .unwrap();
                let query_duration = stdin_ansi_parser.startup_query_duration();
                send_done_parsing_after_query_timeout(
                    send_input_instructions.clone(),
                    query_duration,
                );
            },
        }
    }
    let mut ansi_stdin_events = vec![];
    loop {
        match os_input.read_from_stdin() {
            Ok(buf) => {
                // Filter out kitty graphics responses from parent terminal before processing
                let buf = filter_kitty_graphics_responses(&buf);
                if buf.is_empty() {
                    continue; // All bytes were filtered out
                }

                {
                    // here we check if we need to parse specialized ANSI instructions sent over STDIN
                    // this happens either on startup (see above) or on SIGWINCH
                    //
                    // if we need to parse them, we do so with an internal timeout - anything else we
                    // receive on STDIN during that timeout is unceremoniously dropped
                    let mut stdin_ansi_parser = stdin_ansi_parser.lock().unwrap();
                    if stdin_ansi_parser.should_parse() {
                        let events = stdin_ansi_parser.parse(buf.clone());
                        if !events.is_empty() {
                            ansi_stdin_events.append(&mut events.clone());
                            let _ = send_input_instructions
                                .send(InputInstruction::AnsiStdinInstructions(events));
                        }
                        continue;
                    }
                }
                if !ansi_stdin_events.is_empty() {
                    stdin_ansi_parser
                        .lock()
                        .unwrap()
                        .write_cache(ansi_stdin_events.drain(..).collect());
                }
                current_buffer.append(&mut buf.to_vec());

                if !explicitly_disable_kitty_keyboard_protocol {
                    // first we try to parse with the KittyKeyboardParser
                    // if we fail, we try to parse normally
                    match KittyKeyboardParser::new().parse(&buf) {
                        Some(key_with_modifier) => {
                            send_input_instructions
                                .send(InputInstruction::KeyWithModifierEvent(
                                    key_with_modifier,
                                    current_buffer.drain(..).collect(),
                                ))
                                .unwrap();
                            continue;
                        },
                        None => {},
                    }
                }

                let maybe_more = false; // read_from_stdin should (hopefully) always empty the STDIN buffer completely
                let mut events = vec![];
                input_parser.parse(
                    &buf,
                    |input_event: InputEvent| {
                        events.push(input_event);
                    },
                    maybe_more,
                );

                let event_count = events.len();
                for (i, input_event) in events.into_iter().enumerate() {
                    if holding_mouse && is_mouse_press_or_hold(&input_event) && i == event_count - 1
                    {
                        let mut poller = os_input.stdin_poller();
                        loop {
                            if poller.ready() {
                                break;
                            }
                            send_input_instructions
                                .send(InputInstruction::KeyEvent(
                                    input_event.clone(),
                                    current_buffer.clone(),
                                ))
                                .unwrap();
                        }
                    }

                    holding_mouse = is_mouse_press_or_hold(&input_event);

                    send_input_instructions
                        .send(InputInstruction::KeyEvent(
                            input_event,
                            current_buffer.drain(..).collect(),
                        ))
                        .unwrap();
                }
            },
            Err(e) => {
                if e == "Session ended" {
                    log::debug!("Switched sessions, signing this thread off...");
                } else {
                    log::error!("Failed to read from STDIN: {}", e);
                }
                let _ = send_input_instructions.send(InputInstruction::Exit);
                break;
            },
        }
    }
}

fn is_mouse_press_or_hold(input_event: &InputEvent) -> bool {
    if let InputEvent::Mouse(mouse_event) = input_event {
        if mouse_event.mouse_buttons.contains(MouseButtons::LEFT)
            || mouse_event.mouse_buttons.contains(MouseButtons::RIGHT)
        {
            return true;
        }
    }
    false
}
