//! This repository's deployment: the Worker script `iroh-do`, whose endpoints run the apps
//! listed here. See `BUILD.md` for the build and the bundle.

iroh_do::worker! { apps: [iroh_do_chat_app::ChatApp, iroh_do_echo::EchoApp] }
