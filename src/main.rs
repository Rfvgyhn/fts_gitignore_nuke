use anyhow::{anyhow, Context};
use clap::{Arg, App};
use crossbeam_deque::Worker;
use ignore::{ gitignore::{Gitignore, GitignoreBuilder}};
use itertools::Itertools;
use num_format::{Locale, ToFormattedString};
use std::{env, fs};
use std::path::PathBuf;
use std::time::Instant;

mod immutable_stack;
use immutable_stack::ImmutableStack;

mod job_system;

fn main() -> anyhow::Result<()> {
    let start = Instant::now();

    // Parse cmdline args
    let matches = App::new("☢️ fts_gitignore_nuke ☢️")
        .version(".1")
        .author("Forrest S. <forrestthewoods@gmail.com>")
        .about("Deletes files hidden by .gitignore files")
        .arg(Arg::with_name("directory")
            .short("d")
            .long("directory")
            .value_name("DIRECTORY")
            .help("Root directory to start search"))
        .arg(Arg::with_name("min_file_size")
            .short("mfs")
            .long("min_file_size")
            .value_name("MIN_FILE_SIZE")
            .help("Minimum size, in bytes, to nuke")
            .default_value("0"))
        .arg(Arg::with_name("num_threads")
            .short("t")
            .long("num_threads")
            .value_name("NUM_THREADS")
            .help("Number of threads to use"))
        .arg(Arg::with_name("benchmark")
            .short("b")
            .long("bench")
            .value_name("BENCHMARK")
            .help("Run benchmark. Auto-quit after walking directory")
            .takes_value(false))
        .arg(Arg::with_name("print_glob_matches")
            .long("print_glob_matches")
            .value_name("PRINT_GLOB_MATCHES")
            .help("Prints which glob and which .gitignore matched each path")
            .takes_value(false))
        .arg(Arg::with_name("include_global_ignore")
            .long("include_global_ignore")
            .value_name("INCLUDE_GLOBAL_IGNORE")
            .help("Include global .gitignore for matches")
            .takes_value(false))
        .arg(Arg::with_name("print_errors")
            .long("print_errors")
            .value_name("PRINT_ERRORS")
            .help("Prints errors if encountered")
            .required(false)
            .takes_value(false))
        .arg(Arg::with_name("root")
            .short("r")
            .long("root")
            .value_name("ROOT")
            .help("Include .gitignores between root and files up to root"))
        .get_matches();

    // Pull data out of args
    let min_filesize_in_bytes : u64 = matches.value_of("min_file_size").unwrap().parse().unwrap();
    let num_threads : usize = matches.value_of("num_threads").map_or_else(||num_cpus::get_physical(), |s| s.parse().unwrap());
    let benchmark_mode = matches.is_present("benchmark");
    let print_glob_matches = matches.is_present("print_glob_matches");
    let include_global_ignore = matches.is_present("include_global_ignore");
    let print_errors = matches.is_present("print_errors") && !benchmark_mode;
    let root : Option<PathBuf> = matches.value_of_os("root").map(|s| s.into()).map(|s:PathBuf| std::fs::canonicalize(s).unwrap());
    let empty_str : String = "".to_owned();

    // Helpers to print Result error if print_errors is true
    let check_error = |e: anyhow::Result<()>| {
        if print_errors {
            if let Err(e) = e {
                println!("Error: [{:#}]", e);
            }
        }
    };

    // Helper to generate a context string if print_errors is true
    macro_rules! path_context {
        ($func:expr, $path:expr) => ({
            if print_errors {
                format!("{} {}", $func, $path.display())
            } else {
                empty_str.clone()
            }
        })
    }



    // Determine starting dir
    let starting_dir : std::path::PathBuf = match matches.value_of_os("directory") {
        Some(path) => env::current_dir()?.join(path),
        None => env::current_dir()?
    };

    // Verify starting dir is valid
    if !starting_dir.exists() {
        return Err(anyhow!("Directory [{:?}] does not exist", starting_dir));
    } else if !starting_dir.is_dir() {
        return Err(anyhow!("[{:?}] is not a directory", starting_dir));
    }
    let starting_dir = std::fs::canonicalize(starting_dir)?;



    // Start .gitignore stack with an empty root
    let ignore_stack = ImmutableStack::new();
    let mut ignore_tip = ignore_stack.clone();

    // Add whitelist to stack
    let ignore_whitelist = GitignoreBuilder::new(starting_dir.clone())
        .add_line(None, "!.git").unwrap()
        .add_line(None, "!.hg").unwrap()
        // TODO: cmdline whitelist?
        .build().unwrap();
    ignore_tip = ignore_tip.push(ignore_whitelist);

    // Add global ignore (if exists)
    let mut global_ignore = ignore_tip.clone();
    if include_global_ignore {
        println!("Adding global ignore");
        let (global_gitignore, err) = GitignoreBuilder::new(starting_dir.clone()).build_global();
        if err.is_none() && global_gitignore.num_ignores() > 0 {
            ignore_tip = ignore_stack.push(global_gitignore);
            global_ignore = ignore_tip.clone();
        }
    }

    // Search for ignores in parent directories
    // Stop if .git or .hg is present
    if root.is_some() {
        let mut parent_ignores : Vec<_>  = Default::default();
        let mut dir : &std::path::Path = &starting_dir;
        while let Some(parent_path) = dir.parent() {
            let ignore_path = parent_path.join(".gitignore");
            if ignore_path.exists() {
                let mut ignore_builder = GitignoreBuilder::new(parent_path);
                ignore_builder.add(ignore_path.clone());
                if let Ok(ignore) = ignore_builder.build() {
                    println!("Adding: [{}]", ignore_path.display());
                    parent_ignores.push(ignore);
                }
            }

            // Stop at source control roots
            if parent_path.join(".git").exists() || parent_path.join(".hg").exists() {
                break;
            }

            // Stop at specified root
            if let Some(root) = &root {
                if root == parent_path {
                    break;
                }
            }

            dir = parent_path;
        }

        // Push parent ignores onto ignore_stack
        for ignore in parent_ignores.into_iter().rev() {
            ignore_tip = ignore_stack.push(ignore);
        }
    }


    // Recursive job takes a path, checks if it's ignored, and recurses into subdirs if needed
    // Return value is result for the path only. Sub-directories will run separately
    // and return their own result.
    let recursive_job = |(mut ignore_tip, path): (ImmutableStack<Gitignore>, PathBuf), worker: &Worker<_>| -> Option<Vec<PathBuf>> {
        
        let mut job_ignores : Vec<_> = Default::default();

        // Get iterator to directory children
        let read_dir = fs::read_dir(&path).ok()?;

        // Check for source control root
        if path.join(".git").exists() || path.join(".hg").exists() {
            // Reset ignore tip
            ignore_tip = global_ignore.clone();
        }

        // Check for existing of gitignore
        let ignore_path = path.join(".gitignore");
        if ignore_path.exists() {
            let mut ignore_builder = GitignoreBuilder::new(&path);
            ignore_builder.add(ignore_path);
            if let Ok(ignore) = ignore_builder.build() {
                ignore_tip = ignore_tip.push(ignore);
            }
        }

        // Process each child in directory
        for child in read_dir {

            let result = || -> anyhow::Result<()> {
                let child_path = child
                    .with_context(|| path_context!("fs::read_dir", &path))?
                    .path();
                let child_meta = std::fs::metadata(&child_path)
                    .with_context(|| path_context!("fs::metadata", &child_path))?;

                // Test if child_path is ignored, whitelisted, or neither
                // Return first match that is either ignored or whitelisted
                let is_dir = child_meta.is_dir();
                let ignore_match = ignore_tip.iter()
                    .map(|i| i.matched(&child_path, is_dir))
                    .filter(|m| !m.is_none())
                    .next();
                
                // Handle ignored/whitelisted/neither
                match ignore_match {
                    Some(m) => {
                        //  ignored or whitelisted
                        if print_glob_matches {
                            let glob = m.inner().unwrap();
                            println!("Glob [{:?}] from Gitignore [{:?}] matched path [{:?}]",
                                glob.original(), glob.from(), child_path);
                        }
    
                        // Add ignores to the list. Do nothing if whitelisted
                        if m.is_ignore() {
                            job_ignores.push(child_path);
                        } else {
                            assert!(m.is_whitelist());
                        }
                    },
                    None => {
                        // No match, recurse into directories
                        if is_dir {
                            worker.push((ignore_tip.clone(), child_path));
                        }
                    }
                }

                // Child tested
                Ok(())
            }();

            // Print error if child could not be checked
            check_error(result);           
        }
        
        // Return ignored paths for path
        Some(job_ignores)
    };

    // Initialize data
    println!("🔍 scanning for targets from [{:?}]", starting_dir);
    let initial_data = vec!((ignore_tip, starting_dir));

    // Run recursive jobs
    let ignored_paths : Vec<(usize, PathBuf)> = job_system::run_recursive_job(initial_data, recursive_job, num_threads)
        .into_iter()
        .flatten()
        .enumerate()
        .collect();
    
    // Second recursive job to compute size of ignored directories
    let recursive_dir_size_job = |(root_idx, path): (usize, PathBuf), worker: &Worker<_>| -> Option<(usize, u64)> {
        // Get type of path
        let path_meta = fs::metadata(&path).ok()?;

        // If file, return result immediately
        if path_meta.is_file() {
            return Some((root_idx, path_meta.len()));
        }

        // Get director iterator
        let read_dir = fs::read_dir(&path).ok()?;

        // Iterate children
        let mut files_size = 0;
        for child in read_dir {

            let result = || -> anyhow::Result<()> {
                // Ignore errors
                let child_path = child
                    .with_context(|| path_context!("read_dir", &path))?
                    .path();
                let child_meta = fs::metadata(&child_path)
                    .with_context(|| path_context!("fs::metadata", &child_path))?;

                // Accumualte file size
                // Add directories to the worker
                if child_meta.is_file() {
                    files_size += child_meta.len();
                } else {
                    worker.push((root_idx, child_path));
                }

                Ok(())
            }();
            check_error(result);
        }
        
        Some((root_idx, files_size))
    };

    // Compute path sizes
    let dir_sizes = job_system::run_recursive_job(ignored_paths.clone(), recursive_dir_size_job, num_threads);

    // Sum sizes
    let mut ignore_path_sizes : Vec<u64> = Default::default();
    ignore_path_sizes.resize(ignored_paths.len(), 0);
    for (idx, size) in dir_sizes {
        ignore_path_sizes[idx] += size;
    }

    // Sort ignored paths by size
    let final_ignore_paths : Vec<_> = ignored_paths.into_iter()
        .zip(ignore_path_sizes)
        .map(|((_,path), size)| (path, size))
        .filter(|(_, size)| *size >= min_filesize_in_bytes)
        .sorted_by_key(|kvp| kvp.1)
        .collect();

    // No ignores found
    if final_ignore_paths.is_empty() {
        println!("No ignore paths to delete.");
        return Ok(());
    }

    // Print ignores
    let mut total_bytes = 0;
    for (path, bytes) in &final_ignore_paths {
        total_bytes += bytes;
        if !benchmark_mode {
            println!("  {:10} {:?}", pretty_bytes(*bytes), path);
        }
    }
    println!("Total Bytes: {}", total_bytes.to_formatted_string(&Locale::en));
    println!("Time: {:?}", start.elapsed());
    
    // Skip NUKE op in benchmark mode
    if benchmark_mode {
        return Ok(());
    }

    // Verify nuke
    const NUKE_STRING : &str = "NUKE";
    const QUIT_STRING : &str = "QUIT";

    // Helper to remove either a file or a directory
    let remove_path = |path: &std::path::Path| {

        // Try to remove path
        let result = || -> anyhow::Result<()> {
            let meta = fs::metadata(&path)
                .with_context(|| format!("{} {}", "fs::metadata", path.display()))?;
            
            // Remove file or directory
            if meta.is_file() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("{} {}", "fs::remove_file", path.display()))?;
            } else {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("{} {}", "fs::remove_dir_all", path.display()))?;
            }

            Ok(())
        }();

        // Always print removal errors 
        match result {
            Ok(_) => (),
            Err(e) => println!("Error: {}", e)
        }
    };

    // Loop to get confirmation to nuke data or quit
    loop {
        println!("\n⚠️⚠️⚠️ Do you wish to delete? This action can not be undone! ⚠️⚠️⚠️");
        println!("Type {} to proceed, {} to quit:", NUKE_STRING, QUIT_STRING);
        let mut input = String::new();

        std::io::stdin().read_line(&mut input)?;
        let trimmed_input = input.trim();
        if trimmed_input == NUKE_STRING {
            println!("\n☢️☢️☢️ nuclear launch detected ☢️☢️☢️");

            // Delete all the things
            for (path, _) in final_ignore_paths {
                remove_path(&path);
            }

            println!("☠️☠️☠️ nuclear deletion complete ☠️☠️☠️");
            break;
        } 
        else if trimmed_input.eq_ignore_ascii_case(QUIT_STRING) {
            println!("😇😇😇 Nuclear launch aborted. Thank you and have a nice day. 😇😇😇");
            break;
        }
        else {
            println!("Invalid input. Input was [{}] but must exactly match [{}] to irrevocably nuke. Please try again.", trimmed_input, NUKE_STRING);
        }
    }

    // Mission accomplished
    Ok(())
}

// Print u64 bytes value as a suffixed string
fn pretty_bytes(orig_amount: u64) -> String {
    let mut amount = orig_amount;
    let mut order = 0;
    while amount > 1000 {
        amount /= 1000;
        order += 1;
    }

    match order {
        0 => format!("{} b", amount),
        1 => format!("{} Kb", amount),
        2 => format!("{} Mb", amount),
        3 => format!("{} Gb", amount),
        4 => format!("{} Tb", amount),
        5 => format!("{} Pb", amount),
        6 => format!("{} Exa", amount),
        7 => format!("{} Zetta", amount),
        8 => format!("{} Yotta", amount),
        _ => format!("{}", orig_amount)
    }
}
