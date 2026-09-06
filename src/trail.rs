use std::time::Duration;

use bevy::prelude::*;
use bevy_rapier2d::prelude::Velocity;

/// Below this speed nothing is dropped; at `TRAIL_FULL_SPEED` the trail is at
/// `TRAIL_MAX_ALPHA`. The floor sits above `PLAYER_MIN_VELOCITY_X` so a player
/// nudging along the ground does not smear.
const TRAIL_MIN_SPEED: f32 = 260.0;
const TRAIL_FULL_SPEED: f32 = 900.0;
const TRAIL_SPAWN_INTERVAL: Duration = Duration::from_millis(28);
const TRAIL_LIFETIME: Duration = Duration::from_millis(260);
const TRAIL_MAX_ALPHA: f32 = 0.5;
/// Afterimages render just behind whatever dropped them, so the live sprite
/// always stays readable on top of its own trail.
const TRAIL_Z_OFFSET: f32 = -0.1;

pub struct TrailPlugin;

impl Plugin for TrailPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (spawn_afterimages, fade_afterimages));
    }
}

/// Drops fading copies of the entity's sprite while it moves fast, so a fly,
/// a dive, or a full-speed run reads as motion rather than as teleporting.
#[derive(Component)]
pub struct MotionTrail {
    timer: Timer,
}

impl Default for MotionTrail {
    fn default() -> Self {
        Self {
            timer: Timer::new(TRAIL_SPAWN_INTERVAL, TimerMode::Repeating),
        }
    }
}

/// One dropped copy. Carries no physics and no `Player`, so it is invisible to
/// collision, combat, and the trail spawner itself.
#[derive(Component)]
struct Afterimage {
    timer: Timer,
    start_alpha: f32,
}

fn spawn_afterimages(
    mut commands: Commands,
    time: Res<Time>,
    mut movers: Query<(
        &mut MotionTrail,
        &Velocity,
        &Transform,
        &Sprite,
        &TextureAtlas,
        &Handle<Image>,
        &Visibility,
    )>,
) {
    for (mut trail, velocity, transform, sprite, atlas, texture, visibility) in &mut movers {
        let speed = velocity.linvel.length();
        // Respawning players blink via Visibility while invincible; trailing
        // through the hidden phase would draw the player that is not there.
        if speed < TRAIL_MIN_SPEED || visibility == Visibility::Hidden {
            trail.timer.reset();
            continue;
        }
        if !trail.timer.tick(time.delta()).just_finished() {
            continue;
        }

        let strength =
            ((speed - TRAIL_MIN_SPEED) / (TRAIL_FULL_SPEED - TRAIL_MIN_SPEED)).clamp(0.0, 1.0);
        let start_alpha = TRAIL_MAX_ALPHA * strength;
        let mut afterimage_sprite = sprite.clone();
        afterimage_sprite.color = sprite.color.with_a(start_alpha);

        commands.spawn((
            SpriteSheetBundle {
                texture: texture.clone(),
                atlas: atlas.clone(),
                sprite: afterimage_sprite,
                transform: Transform {
                    translation: transform.translation + Vec3::Z * TRAIL_Z_OFFSET,
                    ..*transform
                },
                ..Default::default()
            },
            Afterimage {
                timer: Timer::new(TRAIL_LIFETIME, TimerMode::Once),
                start_alpha,
            },
            Name::new("Afterimage"),
        ));
    }
}

fn fade_afterimages(
    mut commands: Commands,
    time: Res<Time>,
    mut afterimages: Query<(Entity, &mut Afterimage, &mut Sprite)>,
) {
    for (entity, mut afterimage, mut sprite) in &mut afterimages {
        if afterimage.timer.tick(time.delta()).finished() {
            commands.entity(entity).despawn();
            continue;
        }
        let alpha = afterimage.start_alpha * afterimage.timer.fraction_remaining();
        sprite.color = sprite.color.with_a(alpha);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trail_app() -> App {
        let mut app = App::new();
        app.init_resource::<Time>()
            .add_systems(Update, (spawn_afterimages, fade_afterimages));
        app
    }

    fn spawn_mover(app: &mut App, speed: f32) -> Entity {
        app.world
            .spawn((
                MotionTrail::default(),
                Velocity::linear(Vec2::new(speed, 0.0)),
                Transform::from_xyz(0.0, 0.0, 2.0),
                Sprite::default(),
                TextureAtlas::default(),
                Handle::<Image>::default(),
                Visibility::Visible,
            ))
            .id()
    }

    fn advance(app: &mut App, delta: Duration) {
        app.world.resource_mut::<Time>().advance_by(delta);
        app.update();
    }

    fn afterimage_count(app: &mut App) -> usize {
        app.world.query::<&Afterimage>().iter(&app.world).count()
    }

    #[test]
    fn a_slow_mover_leaves_no_trail() {
        let mut app = trail_app();
        spawn_mover(&mut app, TRAIL_MIN_SPEED - 1.0);

        advance(&mut app, TRAIL_SPAWN_INTERVAL * 4);

        assert_eq!(afterimage_count(&mut app), 0);
    }

    #[test]
    fn a_fast_mover_drops_afterimages_behind_it() {
        let mut app = trail_app();
        let mover = spawn_mover(&mut app, TRAIL_FULL_SPEED);

        advance(&mut app, TRAIL_SPAWN_INTERVAL);

        let mover_z = app.world.get::<Transform>(mover).unwrap().translation.z;
        let (transform, sprite) = app
            .world
            .query_filtered::<(&Transform, &Sprite), With<Afterimage>>()
            .single(&app.world);
        assert!(transform.translation.z < mover_z);
        assert_eq!(sprite.color.a(), TRAIL_MAX_ALPHA);
    }

    #[test]
    fn afterimage_alpha_scales_with_speed() {
        let mut app = trail_app();
        spawn_mover(&mut app, (TRAIL_MIN_SPEED + TRAIL_FULL_SPEED) / 2.0);

        advance(&mut app, TRAIL_SPAWN_INTERVAL);

        let sprite = app
            .world
            .query_filtered::<&Sprite, With<Afterimage>>()
            .single(&app.world);
        assert!((sprite.color.a() - TRAIL_MAX_ALPHA / 2.0).abs() < 0.01);
    }

    #[test]
    fn afterimages_fade_out_and_despawn() {
        let mut app = trail_app();
        let mover = spawn_mover(&mut app, TRAIL_FULL_SPEED);
        advance(&mut app, TRAIL_SPAWN_INTERVAL);
        assert_eq!(afterimage_count(&mut app), 1);

        // Stop the mover so no further afterimages are dropped while the
        // existing one ages out.
        *app.world.get_mut::<Velocity>(mover).unwrap() = Velocity::zero();
        advance(&mut app, TRAIL_LIFETIME / 2);
        let faded = app
            .world
            .query_filtered::<&Sprite, With<Afterimage>>()
            .single(&app.world)
            .color
            .a();
        assert!(faded > 0.0 && faded < TRAIL_MAX_ALPHA);

        advance(&mut app, TRAIL_LIFETIME);
        assert_eq!(afterimage_count(&mut app), 0);
    }
}
