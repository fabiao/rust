#![forbid(unsafe_op_in_unsafe_fn)]

cfg_select! {
    unix => {
        mod unix;
        pub use unix::{Pipe, pipe};
    }
    windows => {
        mod windows;
        pub use windows::{Pipe, pipe};
    }
    target_os = "motor" => {
        mod motor;
        pub use motor::{Pipe, pipe};
    }
    target_os = "ask" => {
        mod ask;
        pub use ask::{Pipe, pipe};
        pub(crate) use ask::{
            accept_reader, accept_reader_from, discard_unclaimed_channels, writer_to_peer,
        };
    }
    _ => {
        mod unsupported;
        pub use unsupported::{Pipe, pipe};
    }
}
