//! Command buffer which is generated by [`load::load_buffer`]
//! Allows working piecemeal to actually place down a map
use std::{collections::HashMap, rc::Rc};

use byondapi::{prelude::*, value::ByondValue};
use dmm_lite::prefabs::{Literal, Prefab};
use eyre::eyre;
use tracy_full::zone;

use crate::{
    _compat::setup_panic_handler,
    load::{
        helpers::{
            ParsedMapTranslationLayer, _bapi_add_turf_to_area, _bapi_apply_preloader,
            _bapi_create_or_get_area, _bapi_create_turf, _bapi_handle_area_contain,
            _bapi_helper_get_world_bounds, _bapi_helper_text2file, _bapi_helper_text2path,
            _bapi_helper_tick_check, _bapi_setup_preloader,
        },
        smart_byond_value::{SharedByondValue, SmartByondValue},
    },
    PARSED_MAPS,
};

#[derive(Debug)]
pub enum Command<'s> {
    CreateArea {
        loc: (usize, usize, usize),
        prefab: &'s Prefab<'s>,
        new_z: bool,
    },
    CreateTurf {
        loc: (usize, usize, usize),
        prefab: &'s Prefab<'s>,
        no_changeturf: bool,
        place_on_top: bool,
    },
    CreateAtom {
        loc: (usize, usize, usize),
        prefab: &'s Prefab<'s>,
    },
}

/// Safety: You're fucked honestly
/// This is extremely dependent on internal BYOND data structures that ~probably~ won't ever change
/// You'll find out it did when byond starts throwing "BAD REF!" internal debug messages (or segfaults)
unsafe fn extremely_unsafe_resolve_coord(
    coord: (usize, usize, usize),
    world_size: (usize, usize, usize),
) -> eyre::Result<ByondValue> {
    zone!("extremely_unsafe_resolve_coord");
    let (max_x, max_y, max_z) = world_size;
    let (x, y, z) = (coord.0 - 1, coord.1 - 1, coord.2 - 1);
    if (0..max_x).contains(&x) && (0..max_y).contains(&y) && (0..max_z).contains(&z) {
        Ok(ByondValue::new_ref(
            ValueType::Turf,
            (x + y * max_x + z * max_x * max_y) as u32,
        ))
    } else {
        Err(eyre!(
            "Attempted to get out-of-range tile at coords {coord:#?}"
        ))
    }
}

/// This thing allows us to cache turfs ahead of time in a safe way,
/// respecting when turf references become invalidated (world.max[x|y|z] changes)
#[derive(Default, Debug)]
pub struct CachedTurfs {
    /// Invalidates cache if this changes
    pub world_bounds: (usize, usize, usize),
    pub cached_turfs: HashMap<(usize, usize, usize), SharedByondValue>,
}

impl CachedTurfs {
    pub fn check_invalidate(&mut self) -> eyre::Result<()> {
        let world_bounds = _bapi_helper_get_world_bounds()?;

        if world_bounds != self.world_bounds {
            self.cached_turfs.clear();
            // Allow ourselves to rebuild the cache if we only invalidate once
            self.world_bounds = world_bounds;
        }

        Ok(())
    }

    /// Caches a turf
    pub fn cache(&mut self, coord: (usize, usize, usize)) -> eyre::Result<()> {
        if let std::collections::hash_map::Entry::Vacant(e) = self.cached_turfs.entry(coord) {
            let turf = unsafe { extremely_unsafe_resolve_coord(coord, self.world_bounds)? };
            e.insert(Rc::new(SmartByondValue::from(turf)));
        }

        Ok(())
    }

    /// Resolves the turf, either by looking it up internally, or failing that, looking it up through byondapi
    /// Will cache byondapi results
    pub fn resolve_coord(&mut self, coord: (usize, usize, usize)) -> eyre::Result<ByondValue> {
        if let Some(turf) = self.cached_turfs.get(&coord) {
            Ok(turf.get_temp_ref())
        } else {
            let turf = unsafe { extremely_unsafe_resolve_coord(coord, self.world_bounds)? };

            self.cached_turfs
                .insert(coord, Rc::new(SmartByondValue::from(turf)));

            Ok(turf)
        }
    }
}

#[derive(Default, Debug)]
pub struct CommandBuffer<'s> {
    pub created_areas: HashMap<&'s str, SharedByondValue>,
    pub known_types: HashMap<&'s str, SharedByondValue>,
    pub cached_turfs: CachedTurfs,
    pub commands: Vec<Command<'s>>,
}

const MIN_PAUSE: usize = 100;

#[byondapi::bind]
pub fn _bapidmm_work_commandbuffer(parsed_map: ByondValue, resume_key: ByondValue) {
    zone!("_bapidmm_work_commandbuffer");
    setup_panic_handler();
    let mut parsed_map = ParsedMapTranslationLayer { parsed_map };
    let id = parsed_map.get_internal_index()? as usize;
    let resume_key = resume_key.get_number()? as usize;

    zone!("borrow parsed_map");
    let internal_data = unsafe { PARSED_MAPS.get_mut() }
        .get_mut(id)
        .ok_or_else(|| eyre!("Bad internal index {id:#?}"))?;

    zone!("borrow internal_data");
    let mut minimum_pause_counter = 0;
    internal_data.with_mut(|all_fields| {
        let command_buffers_map = all_fields.command_buffers;

        zone!("lookup our buffer");
        if let Some(our_command_buffer) = command_buffers_map.get_mut(&resume_key) {
            zone!("command loop");
            let cached_turfs = &mut our_command_buffer.cached_turfs;
            cached_turfs.check_invalidate()?;

            while let Some(command) = our_command_buffer.commands.pop() {
                match command {
                    Command::CreateArea { loc, prefab, new_z } => {
                        zone!("Commmand::CreateArea");

                        let area = if let Some(area) =
                            our_command_buffer.created_areas.get_mut(prefab.0)
                        {
                            area
                        } else {
                            zone!("new area creation");
                            let area = _bapi_create_or_get_area(prefab.0)?;
                            let area = Rc::new(SmartByondValue::from(area));
                            our_command_buffer.created_areas.insert(prefab.0, area);
                            // This can't possibly fail, I hope
                            our_command_buffer.created_areas.get_mut(prefab.0).unwrap()
                        };

                        let area_ref = area.get_temp_ref();
                        let turf_ref = cached_turfs.resolve_coord(loc)?;
                        if turf_ref.is_null() {
                            parsed_map.add_warning(format!(
                                "Unable to create atom at {loc:#?} because coord was null"
                            ))?;
                            continue;
                        }

                        if !new_z {
                            _bapi_handle_area_contain(turf_ref, area_ref)?;
                        }
                        _bapi_add_turf_to_area(area_ref, turf_ref)?;
                    }
                    Command::CreateTurf {
                        loc,
                        prefab,
                        no_changeturf,
                        place_on_top,
                    } => {
                        zone!("Commmand::CreateTurf");
                        let turf_ref = cached_turfs.resolve_coord(loc)?;
                        if turf_ref.is_null() {
                            parsed_map.add_warning(format!(
                                "Unable to create atom at {loc:#?} because coord was null"
                            ))?;
                            continue;
                        }

                        create_turf(
                            &mut parsed_map,
                            turf_ref,
                            prefab,
                            place_on_top,
                            no_changeturf,
                        )?;
                    }
                    Command::CreateAtom { loc, prefab } => {
                        zone!("Commmand::CreateAtom");
                        let turf_ref = cached_turfs.resolve_coord(loc)?;
                        if turf_ref.is_null() {
                            parsed_map.add_warning(format!(
                                "Unable to create atom at {loc:#?} because coord was null"
                            ))?;
                            continue;
                        }
                        create_movable(
                            &mut parsed_map,
                            &mut our_command_buffer.known_types,
                            turf_ref,
                            prefab,
                        )?;
                    }
                }
                minimum_pause_counter += 1;

                // Yield
                if minimum_pause_counter % MIN_PAUSE == 0 && _bapi_helper_tick_check()? {
                    minimum_pause_counter = 0;
                    return Ok(ByondValue::new_num(1.));
                }
            }

            // Clean up after ourselves
            if our_command_buffer.commands.is_empty() {
                zone!("cleanup");
                command_buffers_map.remove(&resume_key);
            }
        }

        zone!("set_loading false and return 0");
        parsed_map.set_loading(false)?;

        Ok(ByondValue::new_num(0.))
    })
}

pub fn create_turf(
    parsed_map: &mut ParsedMapTranslationLayer,
    turf: ByondValue,
    prefab_turf: &dmm_lite::prefabs::Prefab,
    place_on_top: bool,
    no_changeturf: bool,
) -> eyre::Result<ByondValue> {
    zone!("create_turf");
    let (path_text, vars) = prefab_turf;

    zone!("creating path string");
    let vars_list = convert_vars_list_to_byondlist(parsed_map, vars)?;

    _bapi_create_turf(turf, path_text, vars_list, place_on_top, no_changeturf)
}

pub fn create_movable<'s>(
    parsed_map: &mut ParsedMapTranslationLayer,
    path_cache: &mut HashMap<&'s str, SharedByondValue>,
    turf: ByondValue,
    obj: &'s dmm_lite::prefabs::Prefab,
) -> eyre::Result<ByondValue> {
    zone!("movable creation");
    let (path_text, vars) = obj;
    let path = if let Some(path) = path_cache.get(*path_text) {
        path
    } else {
        let path = _bapi_helper_text2path(path_text)?;
        let path = Rc::new(SmartByondValue::from(path));
        path_cache.insert(path_text, path);
        path_cache.get(path_text).unwrap()
    };

    if vars.is_some() {
        let vars_list = convert_vars_list_to_byondlist(parsed_map, vars)?;
        _bapi_setup_preloader(vars_list, path.get_temp_ref())?;
    }

    zone!("byond_new");
    let instance = ByondValue::builtin_new(path.get_temp_ref(), &[turf])?;

    _bapi_apply_preloader(instance)?;

    Ok(instance)
}

fn convert_vars_list_to_byondlist(
    parsed_map: &mut ParsedMapTranslationLayer,
    vars: &Option<Vec<(&str, Literal)>>,
) -> eyre::Result<ByondValue> {
    zone!("convert_vars_list_to_byondlist");
    if let Some(vars) = vars {
        let mut vars_list = ByondValue::new_list()?;
        for (key, literal) in vars {
            let value = convert_literal_to_byondvalue(parsed_map, key, literal)?;
            vars_list.write_list_index(ByondValue::new_str(*key)?, value)?;
        }
        Ok(vars_list)
    } else {
        Ok(ByondValue::null())
    }
}

/// This only hard errors when running into an internal BYOND error, such as bad proc, bad value, out of memory, etc
fn convert_literal_to_byondvalue(
    parsed_map: &mut ParsedMapTranslationLayer,
    key: &str,
    literal: &Literal,
) -> eyre::Result<ByondValue> {
    zone!("convert_literal_to_byondvalue");
    Ok(match literal {
        Literal::Number(n) => ByondValue::new_num(*n),
        Literal::String(s) => ByondValue::new_str(*s)?,
        Literal::Path(p) => _bapi_helper_text2path(p)?,
        Literal::File(f) => _bapi_helper_text2file(f)?,
        Literal::Null => ByondValue::null(),
        Literal::Fallback(s) => {
            parsed_map.add_warning(format!(
                "Parser failed to parse value for {:#?} and fellback to string: {s:#?}",
                key
            ))?;
            ByondValue::new_str(*s)?
        }
        Literal::List(l) => {
            zone!("convert_literal_to_byondvalue(list)");
            let mut list = ByondValue::new_list()?;

            for literal in l {
                match convert_literal_to_byondvalue(parsed_map, key, literal) {
                    Ok(item) => list.push_list(item)?,
                    Err(e) => {
                        parsed_map.add_warning(format!(
                            "Inside list inside {:#?}, failed to parse value: {e:#?}",
                            key
                        ))?;
                    }
                }
            }

            list
        }
        Literal::AssocList(map) => {
            zone!("convert_literal_to_byondvalue(assoc list)");
            let mut list = ByondValue::new_list()?;

            for (list_key, list_val) in map.iter() {
                let key_bv = convert_literal_to_byondvalue(parsed_map, key, list_key);
                let val_bv = convert_literal_to_byondvalue(parsed_map, key, list_val);

                match (key_bv, val_bv) {
                    (Ok(key), Ok(val)) => list.write_list_index(key, val)?,
                    (Err(e), _) => parsed_map.add_warning(format!(
                        "Inside assoc list inside {:#?}, failed to parse assoc list key: {e:#?}",
                        key,
                    ))?,
                    (_, Err(e)) => parsed_map.add_warning(format!(
                        "Inside assoc list inside {:#?}, failed to parse assoc list value: {e:#?}",
                        key
                    ))?,
                }
            }

            list
        }
    })
}
