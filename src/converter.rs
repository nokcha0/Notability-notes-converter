mod archive;
mod cli;
mod model;
mod note;
mod render;
mod util;

pub fn run_cli(args: Vec<String>) -> crate::Result<()> {
    cli::run(args)
}
