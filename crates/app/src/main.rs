// SPDX-License-Identifier: Apache-2.0

fn main() {
    if let Err(err) = bookrack_app::run() {
        eprintln!("bookrack-app: {err:#}");
        std::process::exit(1);
    }
}
