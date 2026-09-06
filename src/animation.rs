use std::time::Duration;

use bevy::prelude::*;

#[derive(Component)]
pub struct Animation {
    pub sprites: &'static [usize],
    pub timer: Timer,
}

impl Animation {
    pub fn new(sprites: &'static [usize], delay: Duration) -> Self {
        Self {
            sprites,
            timer: Timer::new(delay, TimerMode::Repeating),
        }
    }

    /// Retimes a running animation in place. Carrying the elapsed time over
    /// means a speed change shifts the cycle rate instead of snapping the
    /// sprite back to the start of the cycle every frame the speed changes.
    pub fn set_cycle_time(&mut self, delay: Duration) {
        if self.timer.duration() == delay {
            return;
        }
        let elapsed = self.timer.elapsed().min(delay);
        self.timer.set_duration(delay);
        self.timer.set_elapsed(elapsed);
    }
}

pub struct AnimationPlugin;

impl Plugin for AnimationPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, animate);
    }
}

fn animate(mut query: Query<(&mut TextureAtlas, &mut Animation)>, time: Res<Time>) {
    for (mut sprite, mut animation) in query.iter_mut() {
        if animation.timer.tick(time.delta()).just_finished() {
            let current_idx = animation
                .sprites
                .iter()
                .position(|s| *s == sprite.index)
                .unwrap_or(0); // default to 0 if the current sprite is not in the set

            let next_idx = (current_idx + animation.timer.times_finished_this_tick() as usize)
                % animation.sprites.len();

            sprite.index = animation.sprites[next_idx];
        }
    }
}
