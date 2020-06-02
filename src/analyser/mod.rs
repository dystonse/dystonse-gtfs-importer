mod count;
mod curves;
mod visual_schedule;

use chrono::NaiveDateTime;
use clap::{App, Arg, ArgMatches};
use gtfs_structures::Gtfs;
use mysql::*;
use regex::Regex;
use simple_error::SimpleError;

use count::*;
use curves::CurveCreator;
use visual_schedule::*;

use crate::FnResult;
use crate::Main;

use std::str::FromStr;

pub struct Analyser<'a> {
    #[allow(dead_code)]
    main: &'a Main,
    args: &'a ArgMatches,
    data_dir: Option<String>,
}

impl<'a> Analyser<'a> {
    pub fn get_subcommand() -> App<'a> {
        App::new("analyse")
            .subcommand(App::new("count")
                .arg(Arg::with_name("interval")
                    .short('i')
                    .long("interval")
                    .default_value("1h")
                    .help("Sets the step size for counting entries. The value will be parsed by the `parse_duration` crate, which acceps a superset of the `systemd.time` syntax.")
                    .value_name("INTERVAL")
                    .takes_value(true)
                )
            )
            .subcommand(App::new("graph")
                .about("Draws graphical schedules of planned and actual departures.")
                .arg(Arg::with_name("schedule")
                    .short('s')
                    .long("schedule")
                    .required(true)
                    .help("The path of the GTFS schedule that is used as a base for the graphical schedule.")
                    .takes_value(true)
                    .value_name("GTFS_SCHEDULE")
                ).arg(Arg::with_name("route-ids")
                    .short('r')
                    .long("route-ids")
                    .help("If provided, graphical schedules will be created for each route variant of each of the selected routes.")
                    .value_name("ROUTE_ID")
                    .multiple(true)
                    .conflicts_with("shape-ids")
                ).arg(Arg::with_name("shape-ids")
                    .short('p')
                    .long("shape-ids")
                    .help("If provided, graphical schedules will be created for each route variant that has the selected shape-id.")
                    .value_name("SHAPE_ID")
                    .multiple(true)
                    .conflicts_with("route-ids")
                ).arg(Arg::with_name("all")
                    .short('a')
                    .long("all")
                    .help("If provided, graphical schedules will be created for each route of the schedule.")
                    .conflicts_with("route-ids")
                )
            )
            .subcommand(App::new("curves")
                .about("Generates curves just because")
                .arg(Arg::with_name("schedule")
                    .short('s')
                    .long("schedule")
                    .required(true)
                    .help("The path of the GTFS schedule that is used as a base for the curves.")
                    .takes_value(true)
                    .value_name("GTFS_SCHEDULE")
                ).arg(Arg::with_name("route-ids")
                    .short('r')
                    .long("route-ids")
                    .help("If provided, curves will be created for each route variant of each of the selected routes.")
                    .value_name("ROUTE_ID")
                    .multiple(true)
                    .conflicts_with("shape-ids")
                ).arg(Arg::with_name("shape-ids")
                    .short('p')
                    .long("shape-ids")
                    .help("If provided, curves will be created for each route variant that has the selected shape-id.")
                    .value_name("SHAPE_ID")
                    .multiple(true)
                    .conflicts_with("route-ids")
                ).arg(Arg::with_name("all")
                    .short('a')
                    .long("all")
                    .help("If provided, curves will be created for each route of the schedule.")
                    .conflicts_with("route-ids")
                )
            )
            .arg(Arg::with_name("dir")
                .index(1)
                .value_name("DIRECTORY")
                .required_unless("help")
                .help("The directory which contains schedules and realtime data")
                .long_help(
                    "The directory that contains the schedules (located in a subdirectory named 'schedules') \
                    and realtime data (located in a subdirectory named 'rt')."
                )
            )
    }

    pub fn new(main: &'a Main, args: &'a ArgMatches) -> Analyser<'a> {
        Analyser {
            main,
            args,
            data_dir: Some(String::from(args.value_of("dir").unwrap())),
        }
    }

    /// Runs the actions that are selected via the command line args
    pub fn run(&mut self) -> FnResult<()> {
        match self.args.clone().subcommand() {
            ("count", Some(_sub_args)) => run_count(&self),
            ("graph", Some(sub_args)) => {
                let mut vsc = VisualScheduleCreator { 
                    main: self.main, 
                    analyser: self,
                    args: sub_args,    
                    schedule: self.read_schedule(sub_args)?
                };
                vsc.run_visual_schedule()
            }
            ("curves", Some(sub_args)) => {
                let cc = CurveCreator {
                    main: self.main,
                    analyser: self,
                    args: sub_args, 
                    schedule: self.read_schedule(sub_args)?
                };
                cc.run_curves()
            },
            _ => panic!("Invalid arguments."),
        }
    }

    fn read_schedule(&self, sub_args: &ArgMatches) -> FnResult<Gtfs> {
        println!("Parsing schedule…");
        let schedule = Gtfs::new(sub_args.value_of("schedule").unwrap())?; // TODO proper error message if this fails
        println!("Done with parsing schedule.");
        Ok(schedule)
    }

    pub fn date_time_from_filename(filename: &str) -> FnResult<NaiveDateTime> {
        lazy_static! {
            static ref FIND_DATE: Regex = Regex::new(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})").unwrap(); // can't fail because our hard-coded regex is known to be ok
        }
        let date_element_captures =
            FIND_DATE
                .captures(&filename)
                .ok_or(SimpleError::new(format!(
                "File name does not contain a valid date (does not match format YYYY-MM-DD): {}",
                filename
            )))?;
        Ok(NaiveDateTime::from_str(&date_element_captures[1])?)
    }

    // This method was used to output a graphviz dot file of the stops of a route and its variants
    fn print_pair(schedule: &Gtfs, first_stop_id: &str, second_stop_id: &str, reverse: bool) {
        println!(
            r##""{} ()" -> "{} ()" [color={}, dir={}]"##,
            schedule.get_stop(first_stop_id).unwrap().name,
            //first_stop_id,
            schedule.get_stop(second_stop_id).unwrap().name,
            //second_stop_id,
            if reverse { "red" } else { "blue" },
            if reverse { "back" } else { "foreward" }
        );
    }
}