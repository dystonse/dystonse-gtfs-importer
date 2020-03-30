use simple_error::SimpleError;
use std::error::Error;
use std::fs;
use std::fs::DirBuilder;
use std::path::{Path, PathBuf};
use std::{thread, time};
#[macro_use]
extern crate lazy_static;

use chrono::NaiveDate;
use clap::{App, Arg, ArgMatches};
use gtfs_structures::Gtfs;
use mysql::*;
use rayon::prelude::*;
use regex::Regex;
use retry::delay::Fibonacci;
use retry::retry;

mod importer;
use importer::Importer;

// This is handy, because mysql defines its own Result type and we don't
// want to repeat std::result::Result
type FnResult<R> = std::result::Result<R, Box<dyn Error>>;

const TIME_BETWEEN_DIR_SCANS: time::Duration = time::Duration::from_secs(60);

struct Main {
    verbose: bool,
    pool: Pool,
    args: ArgMatches,
    source: String,
}

fn main() -> FnResult<()> {
    let instance = Main::new()?;
    instance.run()?;
    Ok(())
}

fn parse_args() -> ArgMatches {
    let matches = App::new("Dystonse GTFS Importer")
        .subcommand(App::new("automatic")
            .about("Runs forever, importing all files which are present or become present during the run.")
            .arg(Arg::with_name("dir")
                .index(1)
                .value_name("DIRECTORY")
                .required_unless("help")
                .help("The directory which contains schedules and realtime data")
                .long_help(
                    "The directory that contains the schedules (located in a subdirectory named 'schedules') \
                    and realtime data (located in a subdirectory named 'rt'). \
                    Successfully processed files are moved to a subdirectory named 'imported'. \
                    The 'imported' subdirectory will be created automatically if it doesn't already exist."
                )
            )
        )
        .subcommand(App::new("batch")
            .about("Imports all files which are present at the time it is started.")
            .arg(Arg::with_name("dir")
                .index(1)
                .value_name("DIRECTORY")
                .required_unless("help")
                .help("The directory which contains schedules and realtime data")
                .long_help(
                    "The directory that contains the schedules (located in a subdirectory named 'schedules') \
                    and realtime data (located in a subdirectory named 'rt'). \
                    Successfully processed files are moved to a subdirectory named 'imported'. \
                    The 'imported' subdirectory will be created automatically if it doesn't already exist."
                )
            )
        )
        .subcommand(App::new("manual")
            .about("Imports all specified realtime files using one specified schedule. Paths to schedule and realtime files have to be given as arguments.")
            .arg(Arg::with_name("schedule")
                .index(1)
                .value_name("SCHEDULE")
                .help("The static GTFS schedule, as directory or .zip")
                .required_unless("help")
            ).arg(Arg::with_name("rt")
                .index(2)
                .multiple(true)
                .value_name("PBs")
                .help("One or more files with real time data, as .pb or .zip")
                .required_unless("help")
            )
        ).arg(Arg::with_name("verbose")
            .short('v')
            .long("verbose")
            .help("Output status messages during run.")
        ).arg(Arg::with_name("password")
            .short('p')
            .long("password")
            .env("DB_PASSWORD")
            .takes_value(true)
            .help("Password used to connect to the database.")
            .required_unless("help")
        ).arg(Arg::with_name("user")
            .short('u')
            .long("user")
            .env("DB_USER")
            .takes_value(true)
            .help("User on the database.")
            .default_value("dystonse")
        ).arg(Arg::with_name("host")
            .long("host")
            .env("DB_HOST")
            .takes_value(true)
            .help("Host on which the database can be connected.")
            .default_value("localhost")   
        ).arg(Arg::with_name("port")
            .long("port")
            .env("DB_PORT")
            .takes_value(true)
            .help("Port on which the database can be connected.")
            .default_value("3306")
        ).arg(Arg::with_name("database")
            .short('d')
            .long("database")
            .env("DB_DATABASE")
            .takes_value(true)
            .help("Database name which will be selected.")
            .default_value("dystonse")
        ).arg(Arg::with_name("source")
            .short('s')
            .long("source")
            .env("GTFS_DATA_SOURCE_ID")
            .takes_value(true)
            .help("Source identifier for the data sets. Used to distinguish data sets with non-unique ids.")
            .required_unless("help")
            .takes_value(true)
        )
        .get_matches();
    return matches;
}

impl Main {
    /// Constructs a new instance of Main, with parsed arguments and a ready-to-use pool of database connections.
    fn new() -> FnResult<Main> {
        let args = parse_args();
        let verbose = args.is_present("verbose");
        let source = String::from(args.value_of("source").unwrap()); // already validated by clap

        if verbose {
            println!("Connecting to database…");
        }
        let pool = retry(Fibonacci::from_millis(1000), || Main::open_db(&args, verbose))
            .expect("DB connections should succeed eventually.");
        Ok(Main {
            args,
            verbose,
            pool,
            source,
        })
    }

    /// Opens a connection to a database and returns the resulting connection pool.
    /// Takes configuration values from DB_PASSWORD, DB_USER, DB_HOST, DB_PORT and DB_DATABASE
    /// environment variables. For all values except DB_PASSWORD a default is provided.
    fn open_db(args: &ArgMatches, verbose: bool) -> FnResult<Pool> {
        if verbose { println!("Trying to connect to the database."); }
        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            args.value_of("user").unwrap(), // already validated by clap
            args.value_of("password").unwrap(), // already validated by clap
            args.value_of("host").unwrap(), // already validated by clap
            args.value_of("port").unwrap(), // already validated by clap
            args.value_of("database").unwrap()  // already validated by clap
        );
        let pool = Pool::new(url)?;
        Ok(pool)
    }

    /// Runs the actions that are selected via the command line args
    fn run(&self) -> FnResult<()> {
        match self.args.subcommand() {
            ("automatic", Some(sub_args)) => self.run_as_non_manual(sub_args, true),
            ("batch", Some(sub_args)) => self.run_as_non_manual(sub_args, false),
            ("manual", Some(sub_args)) => self.run_as_manual(sub_args),
            _ => panic!("Invalid arguments."),
        }
    }

    /// Handle manual mode
    fn run_as_manual(&self, args: &ArgMatches) -> FnResult<()> {
        let gtfs_schedule_filename = args.value_of("schedule").unwrap(); // already validated by clap
        let gtfs_realtime_filenames: Vec<String> = args
            .values_of("rt")
            .unwrap() // already validated by clap
            .map(|s| String::from(s))
            .collect();
        self.process_schedule_and_realtimes(
            &gtfs_schedule_filename,
            &gtfs_realtime_filenames,
            None,
            None,
        )?;

        Ok(())
    }

    /// Reads contents of the given directory and returns an alphabetically sorted list of included files / subdirectories as Vector of Strings.
    fn read_dir_simple(path: &str) -> FnResult<Vec<String>> {
        let mut path_list: Vec<String> = fs::read_dir(path)?
            .filter_map(|r| r.ok()) // unwraps Options and ignores any None values
            .map(|d| {
                String::from(d.path().to_str().expect(&format!(
                    "Found file with invalid UTF8 in file name in directory {}.",
                    &path
                )))
            })
            .collect();
        path_list.sort();
        Ok(path_list)
    }

    fn date_from_filename(filename: &str) -> FnResult<NaiveDate> {
        lazy_static! {
            static ref FIND_DATE: Regex = Regex::new(r"(\d{4})-(\d{2})-(\d{2})").unwrap(); // can't fail because our hard-coded regex is known to be ok
        }
        let date_element_captures =
            FIND_DATE
                .captures(&filename)
                .ok_or(SimpleError::new(format!(
                "File name does not contain a valid date (does not match format YYYY-MM-DD): {}",
                filename
            )))?;
        let date_option = NaiveDate::from_ymd_opt(
            date_element_captures[1].parse().unwrap(), // can't fail because input string is known to be a bunch of decimal digits
            date_element_captures[2].parse().unwrap(), // can't fail because input string is known to be a bunch of decimal digits
            date_element_captures[3].parse().unwrap(), // can't fail because input string is known to be a bunch of decimal digits
        );
        Ok (date_option.ok_or(SimpleError::new(format!("File name does not contain a valid date (format looks ok, but values are out of bounds): {}", filename)))?)
    }

    /// Handle automatic mode and batch mode, which are very similar to each other
    fn run_as_non_manual(&self, args: &ArgMatches, is_automatic: bool) -> FnResult<()> {
        // construct paths of directories
        let dir = args.value_of("dir").unwrap(); // already validated by clap
        let schedule_dir = format!("{}/schedule", dir);
        let rt_dir = format!("{}/rt", dir);
        let target_dir = format!("{}/imported", dir);
        let fail_dir = format!("{}/failed", dir);

        // ensure that the directory exists
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        builder.create(&target_dir)?; // if target dir can't be created, there's no good way to continue execution
        builder.create(&fail_dir)?; // if fail dir can't be created, there's no good way to continue execution
        if is_automatic {
            loop {
                match self.process_all_files(&schedule_dir, &rt_dir, &target_dir, Some(&fail_dir)) {
                    Ok(_) => {
                        if self.verbose {
                            println!("Finished one iteration. Sleeping until next directory scan.");
                        }
                    }
                    Err(e) => eprintln!(
                        "Iteration failed with error: {}. Sleeping until next directory scan.",
                        e
                    ),
                }
                thread::sleep(TIME_BETWEEN_DIR_SCANS);
            }
        } else {
            match self.process_all_files(&schedule_dir, &rt_dir, &target_dir, Some(&fail_dir)) {
                Ok(_) => {
                    if self.verbose {
                        println!("Finished.");
                    }
                }
                Err(e) => eprintln!("Failed with error: {}.", e),
            }

            return Ok(());
        }
    }

    fn process_all_files(
        &self,
        schedule_dir: &String,
        rt_dir: &String,
        target_dir: &String,
        fail_dir: Option<&String>,
    ) -> FnResult<()> {
        if self.verbose {
            println!("Scan directory");
        }
        // list files in both directories
        let mut schedule_filenames = Main::read_dir_simple(&schedule_dir)?;
        let rt_filenames = Main::read_dir_simple(&rt_dir)?;

        if rt_filenames.is_empty() {
            return Err(Box::from(SimpleError::new("No realtime data.")));
        }

        if schedule_filenames.is_empty() {
            return Err(Box::from(SimpleError::new(
                "No schedule data (but real time data is present).",
            )));
        }

        // get the date of the earliest schedule, then reverse the list to start searching with the latest schedule
        let oldest_schedule_date = Main::date_from_filename(&schedule_filenames[0])?;
        schedule_filenames.reverse();

        // data structures to collect the files to work on in the current iteration (one schedule and all its corresponding rt files)
        let mut current_schedule_file = String::new();
        let mut realtime_files_for_current_schedule: Vec<String> = Vec::new();

        // Iterate over all rt files (oldest first), collecting all rt files that belong to the same schedule to process them in batch.
        for rt_filename in rt_filenames {
            let rt_date = match Main::date_from_filename(&rt_filename) {
                Ok(date) => date,
                Err(e) => {
                    match fail_dir {
                        Some(d) => {
                            Main::move_file_to_dir(&rt_filename, d)?;
                            eprintln!("Rt file {} does not contain a valid date and was moved to {}. (Error was {})", rt_filename, d, e);
                        }
                        None => eprintln!(
                            "Rt file {} does not contain a valid date. (Error was {})",
                            rt_filename, e
                        ),
                    }
                    continue;
                }
            };

            if rt_date <= oldest_schedule_date {
                eprintln!(
                    "Realtime data {} is older than any schedule, skipping.",
                    rt_filename
                );
                continue;
            }

            // Look at all schedules (newest first)
            for schedule_filename in &schedule_filenames {
                let schedule_date = match Main::date_from_filename(&schedule_filename) {
                    Ok(date) => date,
                    Err(e) => {
                        match fail_dir {
                            Some(d) => {
                                Main::move_file_to_dir(schedule_filename, d)?;
                                eprintln!("Schedule file {} does not contain a valid date and was moved to {}. (Error was {})", schedule_filename, d, e);
                            }
                            None => eprintln!(
                                "Schedule file {} does not contain a valid date. (Error was {})",
                                schedule_filename, e
                            ),
                        }
                        continue;
                    }
                };
                // Assume we found the right schedule if this is the newest schedule that is older than the realtime file:
                if rt_date > schedule_date {
                    // process the current schedule's collection before going to next schedule
                    if *schedule_filename != current_schedule_file {
                        if !realtime_files_for_current_schedule.is_empty() {
                            self.process_schedule_and_realtimes(
                                &current_schedule_file,
                                &realtime_files_for_current_schedule,
                                Some(target_dir),
                                fail_dir,
                            )
                            .ok(); // in case of error just go on
                        }
                        // go on with the next schedule
                        current_schedule_file = schedule_filename.clone();
                        realtime_files_for_current_schedule.clear();
                    }
                    realtime_files_for_current_schedule.push(rt_filename.clone());
                    // Correct schedule found for this one, so continue with next realtime file
                    break;
                }
            }
        }

        // process last schedule's collection
        if !realtime_files_for_current_schedule.is_empty() {
            self.process_schedule_and_realtimes(
                &current_schedule_file,
                &realtime_files_for_current_schedule,
                Some(target_dir),
                fail_dir,
            )
            .ok(); // in case of error just go on
        }

        Ok(())
    }

    /// Perform the import of one or more realtime data sets relating to a single schedule
    fn process_schedule_and_realtimes(
        &self,
        gtfs_schedule_filename: &str,
        gtfs_realtime_filenames: &Vec<String>,
        target_dir: Option<&String>,
        fail_dir: Option<&String>,
    ) -> FnResult<()> {
        if self.verbose {
            println!("Parsing schedule…");
        }

        let schedule = match Gtfs::new(gtfs_schedule_filename) {
            Ok(schedule) => schedule,
            Err(e) => {
                match fail_dir {
                    Some(d) => {
                        Main::move_file_to_dir(gtfs_schedule_filename, d)?;
                        eprintln!("Schedule file {} could not be parsed and was moved to {}. (Error was {})", gtfs_schedule_filename, d, e);
                    }
                    None => eprintln!(
                        "Schedule file {} could not be parsed. (Error was {})",
                        gtfs_schedule_filename, e
                    ),
                }
                return Err(Box::from(SimpleError::new(
                    "Schedule file could not be parsed.",
                )));
            }
        };

        if self.verbose {
            println!("Importing realtime data…");
        }
        // create importer for this schedule and iterate over all given realtime files
        let imp = Importer::new(&schedule, &self.pool, self.verbose, &self.source);

        gtfs_realtime_filenames
            .par_iter()
            .for_each(|gtfs_realtime_filename| {
                match self.process_realtime(&gtfs_realtime_filename, &imp, target_dir) {
                    Ok(_) => (),
                    Err(e) => eprintln!("Error while reading {}: {}", &gtfs_realtime_filename, e),
                }
            });
        if self.verbose {
            println!("Done!");
        }
        Ok(())
    }

    /// Process a single realtime file on the given Importer
    fn process_realtime(
        &self,
        gtfs_realtime_filename: &str,
        imp: &Importer,
        target_dir: Option<&String>,
    ) -> FnResult<()> {
        imp.import_realtime_into_database(&gtfs_realtime_filename)?; // assume that the error is temporary, so that we can retry this import in the next iteration
        if self.verbose {
            println!("Finished importing file: {}", &gtfs_realtime_filename);
        } else {
            println!("{}", &gtfs_realtime_filename);
        }
        // move file into target_dir if target_dir is defined
        if let Some(dir) = target_dir {
            Main::move_file_to_dir(gtfs_realtime_filename, dir)?;
        }
        Ok(())
    }

    fn move_file_to_dir(filename: &str, dir: &String) -> FnResult<()> {
        let mut target_path = PathBuf::from(dir);
        target_path.push(Path::new(&filename).file_name().unwrap()); // assume that the filename does not end in `..` because we got it from a directory listing
        std::fs::rename(filename, target_path)?;
        Ok(())
    }
}
