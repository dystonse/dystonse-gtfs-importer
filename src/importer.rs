use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use gtfs_rt::FeedMessage as GtfsRealtimeMessage;
use gtfs_structures::Gtfs;
use gtfs_structures::Trip as ScheduleTrip;
use mysql::prelude::*;
use mysql::*;
use prost::Message; // need to use this, otherwise GtfsRealtimeMessage won't have a `decode` method
use simple_error::SimpleError;
use std::fs::File;
use std::io::prelude::*;

use crate::FnResult;

const MAX_BATCH_SIZE: usize = 1000;

pub struct Importer<'a> {
    pool: &'a Pool,
    gtfs_schedule: &'a Gtfs,
    verbose: bool,
    source: &'a str,
}

enum EventType {
    Arrival,
    Departure,
}

struct EventTimes {
    schedule: Option<i64>,
    estimate: Option<i64>,
    delay: Option<i64>,
}

impl EventTimes {
    fn empty() -> EventTimes {
        EventTimes {
            schedule: None,
            estimate: None,
            delay: None,
        }
    }
}

struct BatchedInsertions<'a> {
    params_vec: Vec<Params>,
    conn: &'a mut PooledConn,
    statement: Statement,
}

impl<'a> Importer<'a> {
    pub fn new(
        gtfs_schedule: &'a Gtfs,
        pool: &'a Pool,
        verbose: bool,
        source: &'a str,
    ) -> Importer<'a> {
        Importer {
            gtfs_schedule,
            pool,
            verbose,
            source,
        }
    }

    pub fn import_realtime_into_database(&self, path: &str) -> FnResult<((u32, u32), (u32, u32))> {
        let mut file = File::open(path)?;
        let mut vec = Vec::<u8>::new();
        if path.ends_with(".zip") {
            let mut archive = zip::ZipArchive::new(file).unwrap();
            let mut zipped_file = archive.by_index(0).unwrap();
            if self.verbose {
                println!("Reading {} from zip…", zipped_file.name());
            }
            zipped_file.read_to_end(&mut vec)?;
        } else {
            file.read_to_end(&mut vec)?;
        }
        // suboptimal, I'd rather not read the whole file into memory, but maybe Prost just works like this
        let message = GtfsRealtimeMessage::decode(&vec)?;
        let time_of_recording = message.header.timestamp.ok_or(SimpleError::new(
            "No global timestamp in realtime data, skipping.",
        ))?;

        // TODO: Remove those statistics, they aren't accurate anyway
        let mut trip_updates_count = 0;
        let mut trip_updates_success_count = 0;
        let mut stop_time_updates_count = 0;
        let mut stop_time_updates_success_count = 0;

        let mut conn = self.pool.get_conn()?;
        let mut batched = BatchedInsertions::new(&mut conn);
        // `message.entity` is actually a collection of entities
        for entity in message.entity {
            if let Some(trip_update) = entity.trip_update {
                trip_updates_count += 1;

                match self.process_trip_update(trip_update, &mut batched, time_of_recording) {
                    Ok((tusc, (stuc, stusc))) => {
                        trip_updates_success_count += tusc;
                        stop_time_updates_count += stuc;
                        stop_time_updates_success_count += stusc;
                    },
                    Err(e) => eprintln!("Error while processing trip update: {}.", e)
                };
            }

            // write the last data rows
            batched.write_to_database()?;
        }

        Ok(((trip_updates_count, trip_updates_success_count), (stop_time_updates_count, stop_time_updates_success_count)))
    }

    fn process_trip_update(
        &self,
        trip_update: gtfs_rt::TripUpdate,
        batched: &mut BatchedInsertions,
        time_of_recording: u64,
    ) -> FnResult<(u32, (u32, u32))> {
        let mut stop_time_updates_count = 0;
        let mut stop_time_updates_success_count = 0;

        let realtime_trip = trip_update.trip;
        let route_id = realtime_trip
            .route_id
            .ok_or(SimpleError::new("Trip needs route_id"))?;
        let trip_id = realtime_trip
            .trip_id
            .ok_or(SimpleError::new("Trip needs id"))?;

        let start_date = if let Some(datestring) = realtime_trip.start_date {
            NaiveDate::parse_from_str(&datestring, "%Y%m%d")?.and_hms(0, 0, 0)
        } else {
            return Err(Box::from(SimpleError::new(
                "Trip without start date. Skipping.",
            )));
        };

        // TODO check if we actually need this
        let realtime_schedule_start_time = if let Some(timestring) = realtime_trip.start_time {
            NaiveTime::parse_from_str(&timestring, "%H:%M:%S")?
        } else {
            return Err(Box::from(SimpleError::new(
                "Trip without start time. Skipping.",
            )));
        };
        let schedule_trip = if let Ok(trip) = self.gtfs_schedule.get_trip(&trip_id) {
            trip
        } else {
            return Err(Box::from(SimpleError::new(format!(
                "Did not find trip {} in schedule. Skipping.",
                trip_id
            ))));
        };

        let schedule_start_time = schedule_trip.stop_times[0].departure_time;
        let time_difference =
            realtime_schedule_start_time.num_seconds_from_midnight() as i32 - schedule_start_time.unwrap() as i32;
        if time_difference != 0 {
            eprintln!("Trip {} has a difference of {} seconds between scheduled start times in schedule data and realtime data.", trip_id, time_difference);
        }

        for stop_time_update in trip_update.stop_time_update {
            stop_time_updates_count += 1;
            stop_time_updates_success_count += self.process_stop_time_update(
                stop_time_update,
                start_date,
                schedule_trip,
                batched,
                &trip_id,
                &route_id,
                time_of_recording,
            )?;
        }

        Ok((1, (stop_time_updates_count, stop_time_updates_success_count)))
    }

    fn process_stop_time_update(
        &self,
        stop_time_update: gtfs_rt::trip_update::StopTimeUpdate,
        start_date: NaiveDateTime,
        schedule_trip: &gtfs_structures::Trip,
        batched: &mut BatchedInsertions,
        trip_id: &String,
        route_id: &String,
        time_of_recording: u64,
    ) -> FnResult<u32> {
        let stop_id = stop_time_update
            .stop_id
            .ok_or(SimpleError::new("no stop_id"))?;
        let stop_sequence = stop_time_update
            .stop_sequence
            .ok_or(SimpleError::new("no stop_sequence"))? as usize;

        let mode = if let Ok(mode_enum) = self.gtfs_schedule.get_route(&route_id) {
            match mode_enum.route_type {
                gtfs_structures::RouteType::Tramway => 0,
                gtfs_structures::RouteType::Subway => 1,
                gtfs_structures::RouteType::Rail => 2,
                gtfs_structures::RouteType::Bus => 3,
                gtfs_structures::RouteType::Ferry => 4,
                gtfs_structures::RouteType::CableCar => 5,
                gtfs_structures::RouteType::Gondola => 6,
                gtfs_structures::RouteType::Funicular => 7,
                gtfs_structures::RouteType::Other(x) => x,
            }
        } else {
            99
        };

        let arrival = Importer::handle_stop_time_update(
            stop_time_update.arrival,
            start_date,
            EventType::Arrival,
            &schedule_trip,
            stop_sequence,
        );
        let departure = Importer::handle_stop_time_update(
            stop_time_update.departure,
            start_date,
            EventType::Departure,
            &schedule_trip,
            stop_sequence,
        );

        batched.add_insertion(Params::from(params! {
            "trip_id" => &trip_id,
            stop_id,
            "route_id" => &route_id,
            stop_sequence,
            time_of_recording,
            mode,
            "time_arrival_schedule" => arrival.schedule,
            "time_arrival_estimate" => arrival.estimate,
            "delay_arrival" => arrival.delay,
            "time_departure_schedule" => departure.schedule,
            "time_departure_estimate" => departure.estimate,
            "delay_departure" => departure.delay,
            "source" => &self.source,
        }))?;

        Ok(1)
    }

    fn handle_stop_time_update(
        event: Option<gtfs_rt::trip_update::StopTimeEvent>,
        start_date: NaiveDateTime,
        event_type: EventType,
        schedule_trip: &ScheduleTrip,
        stop_sequence: usize,
    ) -> EventTimes {
        let delay = if let Some(event) = event {
            if let Some(delay) = event.delay {
                delay as i64
            } else {
                eprintln!(
                    "Stop time update {} without delay. Skipping.",
                    match event_type {
                        EventType::Arrival => "arrival",
                        EventType::Departure => "departure",
                    }
                );
                return EventTimes::empty();
            }
        } else {
            return EventTimes::empty();
        };

        let event_time = match event_type {
            EventType::Arrival => schedule_trip.stop_times[stop_sequence].arrival_time,
            EventType::Departure => schedule_trip.stop_times[stop_sequence].departure_time,
        };
        let schedule = start_date.timestamp() + event_time.expect("no arrival time") as i64;
        let estimate = schedule + delay;

        EventTimes {
            delay: Some(delay),
            schedule: Some(schedule),
            estimate: Some(estimate),
        }
    }
}

impl<'a> BatchedInsertions<'a> {
    fn new(conn: &mut PooledConn) -> BatchedInsertions {
        let statement = conn.prep(r"INSERT INTO `realtime` 
                    (`id`, `trip_id`, `stop_id`, `route_id`, `stop_sequence`, `mode`, `delay_arrival`, `delay_departure`,
                    `time_of_recording`, `time_arrival_schedule`, `time_arrival_estimate`, `time_departure_schedule`, `time_departure_estimate`, `source`) 
                    VALUES 
                    (NULL, :trip_id, :stop_id, :route_id, :stop_sequence, :mode, :delay_arrival, :delay_departure, 
                    FROM_UNIXTIME(:time_of_recording), FROM_UNIXTIME(:time_arrival_schedule), FROM_UNIXTIME(:time_arrival_estimate), FROM_UNIXTIME(:time_departure_schedule), FROM_UNIXTIME(:time_departure_estimate), :source)")
                    .expect("Could not prepare statement"); // Should never happen because of hard-coded statement string

        BatchedInsertions {
            params_vec: Vec::with_capacity(MAX_BATCH_SIZE),
            conn,
            statement,
        }
    }

    fn add_insertion(&mut self, insertion: Params) -> FnResult<()> {
        self.params_vec.push(insertion);
        if self.params_vec.len() > MAX_BATCH_SIZE {
            self.write_to_database()?;
        }
        Ok(())
    }

    fn write_to_database(&mut self) -> FnResult<()> {
        let mut tx = self.conn.start_transaction(TxOpts::default())?;
        tx.exec_batch(&self.statement, &self.params_vec)?;
        self.params_vec.clear();
        tx.commit()?;
        Ok(())
    }
}
