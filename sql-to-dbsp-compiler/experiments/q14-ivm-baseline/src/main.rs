//! The Q14 driver's command line; see `q14_ivm_baseline::driver`.

use anyhow::Result;
use q14_ivm_baseline::driver::{run, Config};

// The generated circuit crates define no allocator, because a binary links
// only one. These are the definitions that the compiler emits at the top of
// every generated circuit, as in the Q3 driver.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

fn main() -> Result<()> {
    run(&Config::from_args(std::env::args().skip(1))?)?;
    Ok(())
}
