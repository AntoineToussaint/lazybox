//! Property tests for framing. Generate arbitrary payloads and assert
//! write → read returns them unchanged, regardless of payload shape.

use lazybox_ipc::{Command, TerminalId, TerminalInputIntent, socket};
use proptest::prelude::*;
use tokio::io::duplex;

fn write_cmd() -> impl Strategy<Value = Command> {
    // Strategy focused on variants with variable-length payloads — those
    // are where framing bugs hide. Scalar variants are covered by the
    // explicit round-trip test.
    prop_oneof![
        any::<Vec<u8>>().prop_map(|bytes| Command::Write {
            terminal_id: TerminalId(1),
            bytes,
            intent: TerminalInputIntent::Compose,
        }),
        (0u16.., 0u16..).prop_map(|(cols, rows)| Command::Resize {
            terminal_id: TerminalId(1),
            cols,
            rows,
        }),
        any::<u64>().prop_map(|id| Command::Close {
            terminal_id: TerminalId(id),
            client_request_id: None,
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any serializable Command round-trips through framed I/O.
    #[test]
    fn arbitrary_command_round_trips(cmd in write_cmd()) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let (mut a, mut b) = duplex(256 * 1024);
            let expected = format!("{cmd:?}");
            socket::write_frame(&mut a, &cmd).await.expect("write");
            drop(a);
            let back: Option<Command> = socket::read_frame(&mut b).await.expect("read");
            let back = back.expect("message present");
            prop_assert_eq!(format!("{back:?}"), expected);
            Ok(())
        }).unwrap();
    }
}
