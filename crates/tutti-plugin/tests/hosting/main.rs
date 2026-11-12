//! Where a plugin runs is its ref's `Hosting` (design 029 step 6b): each value
//! loads that way, and a hosting a format cannot have is refused at the load.

mod catalog;
mod hosting;
