extern crate clap;
extern crate distinst;
extern crate libc;
extern crate pbr;

use clap::{App, Arg, ArgMatches, Values};
use distinst::{
    Config, Disk, DiskError, DiskExt, Disks, FileSystemType, Installer, LvmEncryption,
    PartitionBuilder, PartitionFlag, PartitionInfo, PartitionTable, PartitionType, Sector, Step,
    KILL_SWITCH, PARTITIONING_TEST,
};
use pbr::ProgressBar;

use std::{io, process};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::rc::Rc;
use std::sync::atomic::Ordering;

fn main() {
    let matches = App::new("distinst")
        .arg(
            Arg::with_name("squashfs")
                .short("s")
                .long("squashfs")
                .help("define the squashfs image which will be installed")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("hostname")
                .short("h")
                .long("hostname")
                .help("define the hostname that the new system will have")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("keyboard")
                .short("k")
                .long("keyboard")
                .help("define the keyboard configuration to use")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("lang")
                .short("l")
                .long("lang")
                .help("define the locale that the new system will use")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("remove")
                .short("r")
                .long("remove")
                .help("defines the manifest file that contains the packages to remove post-install")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("disk")
                .short("b")
                .long("block")
                .help("defines a disk that will be manipulated in the installation process")
                .takes_value(true)
                .multiple(true)
                .required(true),
        )
        .arg(
            Arg::with_name("table")
                .short("t")
                .long("new-table")
                .help(
                    "defines a new partition table to apply to the disk, clobbering it in the \
                     process",
                )
                .multiple(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("new")
                .short("n")
                .long("new")
                .help("defines a new partition that will be created on the disk")
                .multiple(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("use")
                .short("u")
                .long("use")
                .help("defines to reuse an existing partition on the disk")
                .takes_value(true)
                .multiple(true),
        )
        .arg(
            Arg::with_name("test")
                .long("test")
                .help("simply test whether the provided arguments pass the partitioning stage"),
        )
        .arg(
            Arg::with_name("delete")
                .short("d")
                .long("delete")
                .help("defines to delete the specified partitions")
                .takes_value(true)
                .multiple(true),
        )
        .arg(
            Arg::with_name("move")
                .short("m")
                .long("move")
                .help("defines to move and/or resize an existing partition")
                .takes_value(true)
                .multiple(true),
        )
        .arg(
            Arg::with_name("logical")
                .long("--logical")
                .help("creates a partition on a LVM volume group")
                .takes_value(true)
                .multiple(true),
        )
        .get_matches();

    if let Err(err) = distinst::log(|_level, message| {
        println!("{}", message);
    }) {
        eprintln!("Failed to initialize logging: {}", err);
    }

    let squashfs = matches.value_of("squashfs").unwrap();
    let hostname = matches.value_of("hostname").unwrap();
    let keyboard = matches.value_of("keyboard").unwrap();
    let lang = matches.value_of("lang").unwrap();
    let remove = matches.value_of("remove").unwrap();

    let pb_opt: Rc<RefCell<Option<ProgressBar<io::Stdout>>>> = Rc::new(RefCell::new(None));

    let res = {
        let mut installer = Installer::default();

        {
            let pb_opt = pb_opt.clone();
            installer.on_error(move |error| {
                if let Some(mut pb) = pb_opt.borrow_mut().take() {
                    pb.finish_println("");
                }

                eprintln!("Error: {:?}", error);
            });
        }

        {
            let pb_opt = pb_opt.clone();
            let mut step_opt = None;
            installer.on_status(move |status| {
                if step_opt != Some(status.step) {
                    if let Some(mut pb) = pb_opt.borrow_mut().take() {
                        pb.finish_println("");
                    }

                    step_opt = Some(status.step);

                    let mut pb = ProgressBar::new(100);
                    pb.show_speed = false;
                    pb.show_counter = false;
                    pb.message(match status.step {
                        Step::Init => "Initializing",
                        Step::Partition => "Partitioning disk ",
                        Step::Extract => "Extracting filesystem ",
                        Step::Configure => "Configuring installation",
                        Step::Bootloader => "Installing bootloader ",
                    });
                    *pb_opt.borrow_mut() = Some(pb);
                }

                if let Some(ref mut pb) = *pb_opt.borrow_mut() {
                    pb.set(status.percent as u64);
                }
            });
        }

        let disks = match configure_disks(&matches) {
            Ok(disks) => disks,
            Err(why) => {
                eprintln!("distinst: invalid disk configuration: {}", why);
                process::exit(1);
            }
        };

        configure_signal_handling();
        if matches.occurrences_of("test") != 0 {
            PARTITIONING_TEST.store(true, Ordering::SeqCst);
        }

        installer.install(
            disks,
            &Config {
                hostname: hostname.into(),
                keyboard: keyboard.into(),
                lang:     lang.into(),
                remove:   remove.into(),
                squashfs: squashfs.into(),
            },
        )
    };

    if let Some(mut pb) = pb_opt.borrow_mut().take() {
        pb.finish_println("");
    }

    let status = match res {
        Ok(()) => {
            println!("install was successful");
            0
        }
        Err(err) => {
            println!("install failed: {}", err);
            1
        }
    };

    process::exit(status);
}

fn configure_signal_handling() {
    extern "C" fn handler(signal: i32) {
        match signal {
            libc::SIGINT => KILL_SWITCH.store(true, Ordering::SeqCst),
            _ => unreachable!(),
        }
    }

    if unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t) == libc::SIG_ERR } {
        eprintln!(
            "distinst: signal handling error: {}",
            io::Error::last_os_error()
        );
        process::exit(1);
    }
}

fn parse_part_type(table: &str) -> PartitionType {
    match table {
        "primary" => PartitionType::Primary,
        "logical" => PartitionType::Logical,
        _ => {
            eprintln!("distinst: partition type must be either 'primary' or 'logical'.");
            exit(1);
        }
    }
}

enum PartType {
    /// A normal partition with a standard file system
    Fs(FileSystemType),
    /// A partition that is formatted with LVM, optionally with encryption.
    Lvm(String, Option<LvmEncryption>),
}

fn parse_fs(fs: &str) -> PartType {
    if fs.starts_with("enc=") {
        let (mut pass, mut keydata) = (None, None);

        let mut fields = fs[4..].split(",");
        let physical_volume = match fields.next() {
            Some(pv) => pv.into(),
            None => {
                eprintln!("distinst: no physical volume was defined in file system field");
                exit(1);
            }
        };

        let volume_group = match fields.next() {
            Some(vg) => vg.into(),
            None => {
                eprintln!("distinst: no volume group was defined in file system field");
                exit(1);
            }
        };

        for field in fields {
            if field.starts_with("pass=") {
                let passval = &field[5..];
                if passval.is_empty() {
                    eprintln!("distinst: provided password is empty");
                    exit(1);
                }

                pass = Some(passval.into());
            } else if field.starts_with("keyfile=") {
                let keyval = &field[8..];
                if keyval.is_empty() {
                    eprintln!("distinst: provided keyval is empty");
                    exit(1);
                }

                keydata = Some(keyval.into());
            } else {
                eprintln!("distinst: invalid fs field");
                exit(1);
            }
        }

        PartType::Lvm(
            volume_group,
            if !pass.is_some() && !keydata.is_some() {
                None
            } else {
                Some(LvmEncryption::new(physical_volume, pass, keydata))
            },
        )
    } else if fs.starts_with("lvm=") {
        let mut fields = fs[4..].split(",");
        PartType::Lvm(
            match fields.next() {
                Some(vg) => vg.into(),
                None => {
                    eprintln!("distinst: no volume group was defined for LVM");
                    exit(1);
                }
            },
            None,
        )
    } else {
        match fs.parse::<FileSystemType>() {
            Ok(fs) => PartType::Fs(fs),
            Err(_) => {
                eprintln!("distinst: provided file system, '{}', was invalid", fs);
                exit(1);
            }
        }
    }
}

fn parse_sector(sector: &str) -> Sector {
    let result = if sector.ends_with("MiB") {
        sector[..sector.len() - 3]
            .parse::<i64>()
            .ok()
            .and_then(|mebibytes| {
                format!("{}M", (mebibytes * 1_048_576) / 1_000_000)
                    .parse::<Sector>()
                    .ok()
            })
    } else {
        sector.parse::<Sector>().ok()
    };

    match result {
        Some(sector) => sector,
        None => {
            eprintln!("distinst: provided sector unit, '{}', was invalid", sector);
            exit(1);
        }
    }
}

fn parse_flags(flags: &str) -> Vec<PartitionFlag> {
    // TODO: implement FromStr for PartitionFlag
    flags
        .split(',')
        .filter_map(|flag| match flag {
            "esp" => Some(PartitionFlag::PED_PARTITION_ESP),
            "boot" => Some(PartitionFlag::PED_PARTITION_BOOT),
            "root" => Some(PartitionFlag::PED_PARTITION_ROOT),
            "swap" => Some(PartitionFlag::PED_PARTITION_SWAP),
            "hidden" => Some(PartitionFlag::PED_PARTITION_HIDDEN),
            "raid" => Some(PartitionFlag::PED_PARTITION_RAID),
            "lvm" => Some(PartitionFlag::PED_PARTITION_LVM),
            "lba" => Some(PartitionFlag::PED_PARTITION_LBA),
            "hpservice" => Some(PartitionFlag::PED_PARTITION_HPSERVICE),
            "palo" => Some(PartitionFlag::PED_PARTITION_PALO),
            "prep" => Some(PartitionFlag::PED_PARTITION_PREP),
            "msft_reserved" => Some(PartitionFlag::PED_PARTITION_MSFT_RESERVED),
            "apple_tv_recovery" => Some(PartitionFlag::PED_PARTITION_APPLE_TV_RECOVERY),
            "diag" => Some(PartitionFlag::PED_PARTITION_DIAG),
            "legacy_boot" => Some(PartitionFlag::PED_PARTITION_LEGACY_BOOT),
            "msft_data" => Some(PartitionFlag::PED_PARTITION_MSFT_DATA),
            "irst" => Some(PartitionFlag::PED_PARTITION_IRST),
            _ => None,
        })
        .collect::<Vec<_>>()
}

fn find_disk_mut<'a>(disks: &'a mut Disks, block: &str) -> &'a mut Disk {
    match disks.find_disk_mut(block) {
        Some(disk) => disk,
        None => {
            eprintln!("distinst: disk '{}' could not be found", block);
            exit(1);
        }
    }
}

fn find_partition_mut<'a>(disk: &'a mut Disk, part_id: i32) -> &'a mut PartitionInfo {
    match disk.get_partition_mut(part_id) {
        Some(partition) => partition,
        None => {
            eprintln!("distinst: partition '{}' was not found", part_id);
            exit(1);
        }
    }
}

fn configure_tables(disks: &mut Disks, tables: Option<Values>) -> Result<(), DiskError> {
    if let Some(tables) = tables {
        for table in tables {
            let values: Vec<&str> = table.split(":").collect();
            eprintln!("table values: {:?}", values);
            if values.len() != 2 {
                eprintln!("distinst: table argument requires two values");
                exit(1);
            }

            match disks.find_disk_mut(values[0]) {
                Some(disk) => match values[1] {
                    "gpt" => disk.mklabel(PartitionTable::Gpt)?,
                    "msdos" => disk.mklabel(PartitionTable::Msdos)?,
                    _ => {
                        eprintln!(
                            "distinst: '{}' is not valid. Value must be either 'gpt' or 'msdos'.",
                            values[1]
                        );
                        exit(1);
                    }
                },
                None => {
                    eprintln!("distinst: '{}' could not be found", values[0]);
                    exit(1);
                }
            }
        }
    }

    Ok(())
}

fn configure_removed(disks: &mut Disks, ops: Option<Values>) -> Result<(), DiskError> {
    if let Some(ops) = ops {
        for op in ops {
            let mut args = op.split(":");
            let block_dev = match args.next() {
                Some(disk) => disk,
                None => {
                    eprintln!("distinst: no block argument provided");
                    exit(1);
                }
            };

            for part in args {
                let part_id = match part.parse::<u32>() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!("distinst: argument is not a valid number");
                        exit(1);
                    }
                };

                let disk = find_disk_mut(disks, block_dev);
                let partition = find_partition_mut(disk, part_id as i32);
                partition.remove();
            }
        }
    }

    Ok(())
}

fn configure_moved(disks: &mut Disks, parts: Option<Values>) -> Result<(), DiskError> {
    if let Some(parts) = parts {
        for part in parts {
            let values: Vec<&str> = part.split(":").collect();
            if values.len() != 4 {
                eprintln!(
                    "distinst: four arguments must be supplied to move operations\n \t-m USAGE: \
                     'block:part_id:start:end"
                );
                exit(1);
            }

            let (block, partition, start, end) = (
                values[0],
                match values[1].parse::<u32>() {
                    Ok(id) => id as i32,
                    Err(_) => {
                        eprintln!("distinst: partition value must be a number");
                        exit(1);
                    }
                },
                match values[2] {
                    "none" => None,
                    value => Some(parse_sector(value)),
                },
                match values[3] {
                    "none" => None,
                    value => Some(parse_sector(value)),
                },
            );

            let disk = find_disk_mut(disks, block);
            if let Some(start) = start {
                let start = disk.get_sector(start);
                disk.move_partition(partition, start)?;
            }

            if let Some(end) = end {
                let end = disk.get_sector(end);
                disk.resize_partition(partition, end)?;
            }
        }
    }

    Ok(())
}

fn configure_reused(disks: &mut Disks, parts: Option<Values>) -> Result<(), DiskError> {
    if let Some(parts) = parts {
        for part in parts {
            let values: Vec<&str> = part.split(":").collect();
            if values.len() < 3 {
                eprintln!(
                    "distinst: three to five colon-delimited values need to be supplied to \
                     --use\n\t-u USAGE: 'part_block:fs-or-reuse:mount[:flags,...]'"
                );
                exit(1);
            } else if values.len() > 5 {
                eprintln!("distinst: too many values were supplied to the use partition flag.");
                exit(1);
            }

            let (block_dev, part_id, fs) = (
                values[0],
                match values[1].parse::<u32>() {
                    Ok(id) => id as i32,
                    Err(_) => {
                        eprintln!("distinst: partition value must be a number");
                        exit(1);
                    }
                },
                match values[2] {
                    "reuse" => None,
                    fs => Some(parse_fs(fs)),
                },
            );

            let (mut key, mut mount, mut flags) = (None, None, None);

            for value in values.iter().skip(3) {
                if value.starts_with("mount=") {
                    mount = Some(Path::new(&value[6..]));
                } else if value.starts_with("flags=") {
                    flags = Some(parse_flags(&value[6..]));
                } else if value.starts_with("keyid=") {
                    key = Some(String::from(&value[6..]));
                } else {
                    eprintln!("distinst: invalid argument supplied: {}", value);
                    exit(1);
                }
            }

            let disk = find_disk_mut(disks, block_dev);
            let partition = find_partition_mut(disk, part_id);

            if let Some(keyid) = key {
                match mount {
                    Some(mount) => partition.set_keydata(keyid, mount.into()),
                    None => {
                        eprintln!("distinst: mount path must be specified with key");
                        exit(1);
                    }
                }
            } else if let Some(mount) = mount {
                partition.set_mount(Path::new(mount).to_path_buf());
            }

            if let Some(fs) = fs {
                let fs = match fs {
                    PartType::Fs(fs) => fs,
                    PartType::Lvm(volume_group, encryption) => {
                        partition.set_volume_group(volume_group, encryption);
                        FileSystemType::Lvm
                    }
                };

                partition.format_with(fs);
            }

            if let Some(flags) = flags {
                partition.flags = flags;
            }
        }
    }

    Ok(())
}

fn configure_new(disks: &mut Disks, parts: Option<Values>) -> Result<(), DiskError> {
    if let Some(parts) = parts {
        for part in parts {
            let values: Vec<&str> = part.split(":").collect();
            if values.len() < 5 {
                eprintln!(
                    "distinst: five to seven colon-delimited values need to be supplied to a new \
                     partition.\n\t-n USAGE: \
                     'block:part_type:start_sector:end_sector:fs[:mount:flags,...]'"
                );
                exit(1);
            } else if values.len() > 7 {
                eprintln!("distinst: too many values were supplied to the new partition flag");
                exit(1);
            }

            let (block, kind, start, end, fs) = (
                values[0],
                parse_part_type(values[1]),
                parse_sector(values[2]),
                parse_sector(values[3]),
                parse_fs(values[4]),
            );

            let (mut key, mut mount, mut flags) = (None, None, None);

            for value in values.iter().skip(5) {
                if value.starts_with("mount=") {
                    mount = Some(Path::new(&value[6..]));
                } else if value.starts_with("flags=") {
                    flags = Some(parse_flags(&value[6..]));
                } else if value.starts_with("keyid=") {
                    key = Some(String::from(&value[6..]));
                } else {
                    eprintln!("distinst: invalid argument supplied: {}", value);
                    exit(1);
                }
            }

            let disk = find_disk_mut(disks, block);

            let start = disk.get_sector(start);
            let end = disk.get_sector(end);
            let mut builder = match fs {
                PartType::Lvm(volume_group, encryption) => {
                    PartitionBuilder::new(start, end, FileSystemType::Lvm)
                        .partition_type(kind)
                        .logical_volume(volume_group, encryption)
                }
                PartType::Fs(fs) => PartitionBuilder::new(start, end, fs).partition_type(kind),
            };

            if let Some(flags) = flags {
                builder = builder.flags(flags);
            }

            if let Some(keyid) = key {
                match mount {
                    Some(mount) => builder = builder.keydata(keyid, mount.into()),
                    None => {
                        eprintln!("distinst: mount path must be specified with key");
                        exit(1);
                    }
                }
            } else if let Some(mount) = mount {
                builder = builder.mount(mount.into());
            }

            disk.add_partition(builder)?;
        }
    }

    Ok(())
}

// Defines a new partition to assign to a volume group
struct LogicalArgs {
    // The group to create a partition on
    group: String,
    // The name of the partition
    name: String,
    // The length of the partition
    size: Sector,
    // The filesystem to assign to this partition
    fs: FileSystemType,
    // Where to mount this partition
    mount: Option<PathBuf>,
    // The partition flags to assign
    flags: Option<Vec<PartitionFlag>>,
}

fn parse_logical<F: FnMut(LogicalArgs)>(values: Values, mut action: F) {
    for value in values {
        let values: Vec<&str> = value.split(":").collect();
        if values.len() < 4 {
            eprintln!("distinst: at least four values need to be supplied for logical volumes");
            exit(1);
        } else if values.len() > 6 {
            eprintln!(
                "distinst: no more than six arguments should be supplied for logical volumes"
            );
            exit(1);
        }

        let (mut mount, mut flags) = (None, None);

        for arg in values.iter().skip(4) {
            if arg.starts_with("mount=") {
                let mountval = &arg[6..];
                if mountval.is_empty() {
                    eprintln!("distinst: mount value is empty");
                    exit(1);
                }

                mount = Some(Path::new(mountval).to_path_buf());
            } else if arg.starts_with("flags=") {
                let flagval = &arg[6..];
                if flagval.is_empty() {
                    eprintln!("distinst: mount value is empty");
                    exit(1);
                }

                flags = Some(parse_flags(flagval));
            } else {
                eprintln!("distinst: invalid field passed to logical volume flag");
                exit(1);
            }
        }

        action(LogicalArgs {
            group: values[0].into(),
            name: values[1].into(),
            size: parse_sector(values[2]),
            fs: match parse_fs(values[3]) {
                PartType::Fs(fs) => fs,
                PartType::Lvm(..) => {
                    unimplemented!("LUKS on LVM is unsupported");
                }
            },
            mount,
            flags,
        });
    }
}

fn configure_lvm(disks: &mut Disks, logical: Option<Values>) -> Result<(), DiskError> {
    if let Some(logical) = logical {
        parse_logical(logical, |args| {
            match disks.find_logical_disk_mut(&args.group) {
                Some(lvm_device) => {
                    let start = match lvm_device.get_partitions().iter().last() {
                        Some(partition) => partition.end_sector + 1,
                        None => 0,
                    };

                    let end = start + lvm_device.get_sector(args.size);
                    let mut builder =
                        PartitionBuilder::new(start, end, args.fs).name(args.name.clone());

                    if let Some(mount) = args.mount.as_ref() {
                        eprintln!("{} was specified as mount", mount.display());
                        builder = builder.mount(mount.clone());
                    }

                    if let Some(flags) = args.flags.as_ref() {
                        builder = builder.flags(flags.clone());
                    }

                    if let Err(why) = lvm_device.add_partition(builder) {
                        eprintln!("distinst: unable to add partition to lvm device: {}", why);
                        exit(1);
                    }
                }
                None => {
                    eprintln!(
                        "distinst: could not find volume group associated with '{}'",
                        args.group
                    );
                    exit(1);
                }
            }
        });
    }

    Ok(())
}

fn configure_disks(matches: &ArgMatches) -> Result<Disks, DiskError> {
    let mut disks = Disks::new();

    for block in matches.values_of("disk").unwrap() {
        eprintln!("distinst: adding {} to disks configuration", block);
        disks.add(Disk::from_name(block)?);
    }

    configure_tables(&mut disks, matches.values_of("table"))?;
    configure_removed(&mut disks, matches.values_of("delete"))?;
    eprintln!("distinst: configuring moved partitions");
    configure_moved(&mut disks, matches.values_of("move"))?;
    eprintln!("distinst: configuring reused partitions");
    configure_reused(&mut disks, matches.values_of("use"))?;
    eprintln!("distinst: configuring new partitions");
    configure_new(&mut disks, matches.values_of("new"))?;
    eprintln!("distinst: initializing LVM groups");
    disks.initialize_volume_groups();
    eprintln!("distinst: configuring LVM devices");
    configure_lvm(&mut disks, matches.values_of("logical"))?;
    eprintln!("distisnt: disks configured");

    Ok(disks)
}
