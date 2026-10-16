//! Everything that talks to mikanani.me: the RSS feed, the search and
//! bangumi page scrapers, and choosing episodes from a feed.

pub mod feed;
pub mod scrape;
pub mod select;

pub use scrape::{Show, Subgroup, search_shows, subgroups};
