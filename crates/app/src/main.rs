// SPDX-License-Identifier: Apache-2.0

fn main() {
    // Not inside `bookrack_app::run`: that is a library function, and a
    // library that loads a file from the working directory is the very
    // shape this call exists to move out of the library layer.
    bookrack_config::load_dotenv();
    if let Err(err) = bookrack_app::run() {
        eprintln!("bookrack-app: {err:#}");
        std::process::exit(1);
    }
}
