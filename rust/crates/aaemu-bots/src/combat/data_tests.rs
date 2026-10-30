//! Unit tests of the combat pieces that need a server's game data: skill info, the data
//! scoring, the learned-skill filter and the target / rotation helpers.

use std::sync::Arc;

use aaemu_game::data::GameData;
use aaemu_game::models::char_data::Skill;
use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::data::effects::{DamageEffect, EffectKind, EffectTemplate};
use aaemu_game::systems::skills::data::templates::SkillEffect;
use aaemu_game::systems::skills::data::SkillTemplate;
use aaemu_game::test_support::{spawn_player, test_character, test_server_with, TestClient};
use aaemu_game::world::PlayerRef;

use super::rotation::choose_rotation;
use super::skills::{build_info, choose_data, Choice, SkillKind};
use super::target;
use super::CombatState;
use crate::rotation::Rotation;

pub(crate) const KILL_SKILL: u32 = 60_001;

/// A server whose game data has one 5 000 damage skill (`KILL_SKILL`) and one slow one.
pub(crate) fn combat_server(slow_skill: Option<u32>) -> Arc<GameServer> {
    let mut data = GameData::empty();
    add_damage_skill(&mut data, KILL_SKILL, 5_000, 25, 0);
    add_damage_skill(&mut data, 3, 50, 4, 0);
    if let Some(id) = slow_skill {
        add_damage_skill(&mut data, id, 5_000, 25, 800);
    }
    let server = Arc::new(test_server_with(data));
    server.set_self();
    server
}

fn add_damage_skill(data: &mut GameData, id: u32, damage: i32, max_range: i32, cast_ms: i32) {
    let effect_id = 900_000 + id;
    let effect = DamageEffect {
        id: effect_id,
        damage_type: 1,
        fixed_min: damage,
        fixed_max: damage,
        multiplier: 1.0,
        use_fixed_damage: true,
        dps_multiplier: 1.0,
        ..Default::default()
    };
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(effect)),
        effect_id,
    );
    let mut template = SkillTemplate {
        id,
        target_type: 4,
        min_range: 0,
        max_range,
        casting_time: cast_ms,
        ..Default::default()
    };
    template.effects.push(SkillEffect {
        effect_id,
        template: Some((EffectKind::DamageEffect, effect_id)),
        start_level: 0,
        end_level: 255,
        chance: 100,
        application_method: 1,
        ..Default::default()
    });
    data.systems.skills.add_skill(template);
}

pub(crate) fn learning_bot(server: &Arc<GameServer>, skills: &[u32]) -> (PlayerRef, TestClient) {
    let client = TestClient::new(server, 1, 1);
    let mut character = test_character(2, "Fighter", 100.0, 100.0);
    character.unit.level = 10;
    for id in skills {
        character.skills.skills.push(Skill { id: *id, level: 1 });
    }
    let player = spawn_player(server, &client, character, 0x100);
    (player, client)
}

#[tokio::test]
async fn the_static_tables_describe_the_damage_skill() {
    let server = combat_server(None);
    let info = build_info(&server, KILL_SKILL).expect("the fixture skill");
    assert_eq!(info.kind, SkillKind::Damage);
    assert!(info.damages);
    assert!(!info.heals);
    assert_eq!(info.max_range, 25.0);
    assert!(
        info.power >= 1.0,
        "fixed damage has a power weight: {}",
        info.power
    );
}

#[tokio::test]
async fn the_data_scoring_casts_a_learned_damage_skill_at_a_monster_in_range() {
    let server = combat_server(None);
    let (player, _client) = learning_bot(&server, &[KILL_SKILL]);
    let npc = target::spawn_monster(&server, 0x300, 900, 2, 500, 103.0, 100.0);
    let target_obj = aaemu_game::world::WorldObject::from(&npc);
    let mut state = CombatState::default();
    let now = aaemu_game::world::now_ms();
    match choose_data(&server, &player, &mut state, &target_obj, 3.0, now) {
        Choice::Cast { skill, self_target } => {
            assert_eq!(skill, KILL_SKILL);
            assert!(!self_target);
        }
        other => panic!("expected a cast, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unlearned_skill_is_not_a_candidate() {
    let server = combat_server(None);
    let (player, _client) = learning_bot(&server, &[]);
    let npc = target::spawn_monster(&server, 0x300, 900, 2, 500, 103.0, 100.0);
    let target_obj = aaemu_game::world::WorldObject::from(&npc);
    let mut state = CombatState::default();
    let now = aaemu_game::world::now_ms();
    let choice = choose_data(&server, &player, &mut state, &target_obj, 3.0, now);
    match choice {
        Choice::Cast { skill, .. } => assert!(
            super::DEFAULT_ATTACKS.contains(&skill),
            "only the default attacks are left, got {skill}"
        ),
        other => panic!("expected a default attack, got {other:?}"),
    }
}

#[tokio::test]
async fn the_rotation_chooses_its_filler_and_respects_range() {
    let server = combat_server(None);
    let (player, _client) = learning_bot(&server, &[KILL_SKILL]);
    let npc = target::spawn_monster(&server, 0x300, 900, 2, 500, 103.0, 100.0);
    let target_obj = aaemu_game::world::WorldObject::from(&npc);
    let mut state = CombatState {
        rotation: Some(Arc::new(
            Rotation::parse(
                r#"{"archetype":"Test","role":"melee_dps","rules":[],
                "fillers":[{"skill":60001,"name":"Hit"}]}"#,
            )
            .expect("rotation"),
        )),
        ..Default::default()
    };
    let now = aaemu_game::world::now_ms();
    assert_eq!(
        choose_rotation(&server, &player, &mut state, &target_obj, 3.0, now),
        Some((KILL_SKILL, false))
    );

    // Out of the skill's reach: the rule's `range`/the gate keep it out.
    state.rotation = Some(Arc::new(
        Rotation::parse(
            r#"{"archetype":"Test","role":"melee_dps","rules":[
                {"id":"close","when":{"range":[0,4]},"then":[{"skill":60001}],"relevance":20}]}"#,
        )
        .expect("rotation"),
    ));
    assert_eq!(
        choose_rotation(&server, &player, &mut state, &target_obj, 30.0, now),
        None
    );
}
