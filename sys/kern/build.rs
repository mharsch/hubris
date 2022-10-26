// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;

use anyhow::{Context, Result};
use serde::Deserialize;

fn main() -> Result<()> {
    build_util::expose_m_profile();

    generate_consts()?;
    generate_statics()?;

    Ok(())
}

fn generate_consts() -> Result<()> {
    let out = build_util::out_dir();
    let mut const_file = File::create(out.join("consts.rs"))
        .context("creating consts.rs file")?;

    writeln!(
        const_file,
        "// See build.rs for an explanation of this constant"
    )?;

    // EXC_RETURN is used on ARMv8m to return from an exception. This value
    // differs between secure and non-secure in two important ways:
    // bit 6 = S = secure or non-secure stack used
    // bit 0 = ES = the security domain the exception was taken to
    // These need to be consistent! The failure mode is a secure fault
    // otherwise
    if let Ok(secure) = build_util::env_var("HUBRIS_SECURE") {
        if secure == "0" {
            writeln!(
                const_file,
                "pub const EXC_RETURN_CONST : u32 = 0xFFFFFFAC;"
            )?;
        } else {
            writeln!(
                const_file,
                "pub const EXC_RETURN_CONST : u32 = 0xFFFFFFED;"
            )?;
        }
    } else {
        writeln!(const_file, "pub const EXC_RETURN_CONST : u32 = 0xFFFFFFED;")?;
    }

    Ok(())
}

fn generate_statics() -> Result<()> {
    let image_id: u64 = build_util::env_var("HUBRIS_IMAGE_ID")?
        .parse()
        .context("parsing HUBRIS_IMAGE_ID")?;
    let kconfig: KernelConfig =
        ron::de::from_str(&build_util::env_var("HUBRIS_KCONFIG")?)
            .context("parsing kconfig from HUBRIS_KCONFIG")?;

    let out = build_util::out_dir();
    let mut file =
        File::create(out.join("kconfig.rs")).context("creating kconfig.rs")?;

    writeln!(file, "// See build.rs for details")?;

    writeln!(file, "#[no_mangle]")?;
    writeln!(file, "pub static HUBRIS_IMAGE_ID: u64 = {};", image_id)?;
    writeln!(
        file,
        "const HUBRIS_TASK_COUNT: usize = {};",
        kconfig.tasks.len()
    )?;

    writeln!(
        file,
        "static HUBRIS_TASK_DESCS: [abi::TaskDesc; HUBRIS_TASK_COUNT] = ["
    )?;
    for task in &kconfig.tasks {
        writeln!(file, "    abi::TaskDesc {{")?;
        writeln!(file, "        regions: [")?;
        for region in &task.regions {
            writeln!(file, "            {},", region)?;
        }
        writeln!(file, "        ],")?;
        writeln!(file, "        entry_point: {:#010x},", task.entry_point)?;
        writeln!(file, "        initial_stack: {:#010x},", task.initial_stack)?;
        writeln!(file, "        priority: {},", task.priority)?;
        writeln!(file, "        index: {},", task.index)?;
        writeln!(
            file,
            "        flags: unsafe {{ \
            abi::TaskFlags::from_bits_unchecked({}) }},",
            task.flags.bits()
        )?;
        writeln!(file, "    }},")?;
    }
    writeln!(file, "];")?;

    writeln!(
        file,
        "static mut HUBRIS_TASK_TABLE_SPACE: \
        core::mem::MaybeUninit<[crate::task::Task; HUBRIS_TASK_COUNT]> = \
        core::mem::MaybeUninit::uninit();",
    )?;

    writeln!(
        file,
        "static mut HUBRIS_REGION_TABLE_SPACE: \
        core::mem::MaybeUninit<[\
            [&'static abi::RegionDesc; abi::REGIONS_PER_TASK]; \
            HUBRIS_TASK_COUNT]> = core::mem::MaybeUninit::uninit();"
    )?;

    writeln!(
        file,
        "static HUBRIS_REGION_DESCS: [abi::RegionDesc; {}] = [",
        kconfig.regions.len()
    )?;
    for region in &kconfig.regions {
        writeln!(file, "    abi::RegionDesc {{")?;
        writeln!(file, "        base: {:#010x},", region.base)?;
        writeln!(file, "        size: {:#010x},", region.size)?;
        writeln!(
            file,
            "        attributes: unsafe {{ \
            abi::RegionAttributes::from_bits_unchecked({}) }},",
            region.attributes.bits()
        )?;
        writeln!(file, "    }},")?;
    }
    writeln!(file, "];")?;

    // Now, we generate two perfect hashes:
    //  irq num => abi::Interrupt
    //  (task, notifications) => abi::InterruptSet
    //
    // The first table allows for efficient implementation of the default
    // interrupt handle, with O(1) lookup of the task which owns a particular
    // interrupt.
    //
    // The second table allows for efficient implementation of `irq_control`,
    // where a task enables or disables one or more IRQS based on notification
    // masks.
    let irq_task_map = kconfig
        .irqs
        .iter()
        .map(|irq| (irq.irq, irq.owner))
        .collect::<Vec<_>>();

    let mut per_task_irqs: HashMap<_, Vec<_>> = HashMap::new();
    for irq in &kconfig.irqs {
        per_task_irqs.entry(irq.owner).or_default().push(irq.irq)
    }
    let task_irq_map = per_task_irqs.into_iter().collect::<Vec<_>>();

    use abi::{InterruptNum, InterruptOwner};
    let fmt_irq_task = |v: Option<&(InterruptNum, InterruptOwner)>| {
        match v {
            Some((irq, owner)) => format!(
                "(abi::InterruptNum({}), abi::InterruptOwner {{ task: {}, notification: 0b{:b} }}),",
                irq.0, owner.task, owner.notification
            ),
            None => "(abi::InterruptNum::invalid(), abi::InterruptOwner::invalid()),"
                .to_string(),
        }
    };
    let fmt_task_irq = |v: Option<&(InterruptOwner, Vec<InterruptNum>)>| {
        match v {
            Some((owner, irqs)) => format!(
                "(abi::InterruptOwner {{ task: {}, notification: 0b{:b} }}, &[{}]),",
                owner.task, owner.notification,
                irqs.iter()
                    .map(|i| format!("abi::InterruptNum({})", i.0))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => {
                "(abi::InterruptOwner::invalid(), &[]),"
                    .to_string()
            }
        }
    };

    let target = build_util::target();
    if target.starts_with("thumbv6m") {
        let task_irq_map = phash_gen::OwnedSortedList::build(task_irq_map)
            .context("building task-to-IRQ map")?;
        let irq_task_map = phash_gen::OwnedSortedList::build(irq_task_map)
            .context("building IRQ-to-task map")?;

        // Generate text for the Interrupt and InterruptSet tables stored in the
        // PerfectHashes
        let irq_task_value = irq_task_map
            .values
            .iter()
            .map(|o| fmt_irq_task(Some(o)))
            .collect::<Vec<String>>()
            .join("\n        ");
        let task_irq_value = task_irq_map
            .values
            .iter()
            .map(|o| fmt_task_irq(Some(o)))
            .collect::<Vec<String>>()
            .join("\n        ");

        write!(file, "
use phash::SortedList;
pub const HUBRIS_IRQ_TASK_LOOKUP: SortedList::<abi::InterruptNum, abi::InterruptOwner> = SortedList {{
    values: &[
        {}
    ],
}};
pub const HUBRIS_TASK_IRQ_LOOKUP: SortedList::<abi::InterruptOwner, &'static [abi::InterruptNum]> = SortedList {{
    values: &[
        {}
    ],
}};",
        irq_task_value, task_irq_value)?;
    } else if target.starts_with("thumbv7m")
        || target.starts_with("thumbv7em")
        || target.starts_with("thumbv8m")
    {
        let nested_import = if let Ok(task_irq_map) =
            phash_gen::OwnedPerfectHashMap::build(task_irq_map.clone())
        {
            let task_irq_value = task_irq_map
                .values
                .iter()
                .map(|o| fmt_task_irq(o.as_ref()))
                .collect::<Vec<String>>()
                .join("\n        ");
            writeln!(file, "
use phash::PerfectHashMap;
pub const HUBRIS_TASK_IRQ_LOOKUP: PerfectHashMap::<'_, abi::InterruptOwner, &'static [abi::InterruptNum]> = PerfectHashMap {{
    m: {:#x},
    values: &[
        {}
    ],
}};",
                task_irq_map.m, task_irq_value)?;
            false
        } else {
            let task_irq_map =
                phash_gen::OwnedNestedPerfectHashMap::build(task_irq_map)
                    .context("building task-to-IRQ perfect hash")?;
            let task_irq_value = task_irq_map
                .values
                .iter()
                .map(|v| {
                    format!(
                        "&[\n            {}\n        ],",
                        v.iter()
                            .map(|o| fmt_task_irq(o.as_ref()))
                            .collect::<Vec<String>>()
                            .join("\n            ")
                    )
                })
                .collect::<Vec<String>>()
                .join("\n        ");
            writeln!(file, "
use phash::NestedPerfectHashMap;
pub const HUBRIS_TASK_IRQ_LOOKUP: NestedPerfectHashMap::<abi::InterruptOwner, &'static [abi::InterruptNum]> = NestedPerfectHashMap {{
    m: {:#x},
    g: &{:#x?},
    values: &[
        {}
    ],
}};",
                task_irq_map.m, task_irq_map.g, task_irq_value)?;
            true
        };

        if let Ok(irq_task_map) =
            phash_gen::OwnedPerfectHashMap::build(irq_task_map.clone())
        {
            if nested_import {
                writeln!(file, "use phash::PerfectHashMap;")?;
            }
            // Generate text for the Interrupt and InterruptSet tables stored in the
            // PerfectHashes
            let irq_task_value = irq_task_map
                .values
                .iter()
                .map(|o| fmt_irq_task(o.as_ref()))
                .collect::<Vec<String>>()
                .join("\n        ");
            writeln!(file, "
pub const HUBRIS_IRQ_TASK_LOOKUP: PerfectHashMap::<'_, abi::InterruptNum, abi::InterruptOwner> = PerfectHashMap {{
    m: {:#x},
    values: &[
        {}
    ],
}};",
                irq_task_map.m, irq_task_value)?;
        } else {
            let irq_task_map =
                phash_gen::OwnedNestedPerfectHashMap::build(irq_task_map)
                    .context("building IRQ-to-task perfect hash")?;
            if !nested_import {
                writeln!(file, "use phash::NestedPerfectHashMap;")?;
            }
            let irq_task_value = irq_task_map
                .values
                .iter()
                .map(|v| {
                    format!(
                        "&[\n            {}\n        ],",
                        v.iter()
                            .map(|o| fmt_irq_task(o.as_ref()))
                            .collect::<Vec<String>>()
                            .join("\n            ")
                    )
                })
                .collect::<Vec<String>>()
                .join("\n        ");
            writeln!(file, "
pub const HUBRIS_IRQ_TASK_LOOKUP: NestedPerfectHashMap::<abi::InterruptNum, abi::InterruptOwner> = NestedPerfectHashMap {{
    m: {:#x},
    g: &{:#x?},
    values: &[
        {}
    ],
}};",
                irq_task_map.m, irq_task_map.g, irq_task_value)?;
        }
    } else {
        panic!("Don't know the target {}", target);
    }

    Ok(())
}

#[derive(Deserialize)]
struct KernelConfig {
    tasks: Vec<abi::TaskDesc>,
    regions: Vec<abi::RegionDesc>,
    irqs: Vec<abi::Interrupt>,
}
