use bevy_utils::tracing::warn;
use bevy_utils::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;

use crate::schedule::{IntoSystemDescriptor, SystemLabel};
use crate::system::{Command, IntoSystem, System, SystemTypeIdLabel};
use crate::world::{Mut, World};
// Needed for derive(Component) macro
use crate as bevy_ecs;
use bevy_ecs_macros::Component;

/// Stores initialized [`Systems`](crate::system::System), so they can be reused and run in an ad-hoc fashion
///
/// Systems are keyed by their [`SystemLabel`]:
///  - all systems with a given label will be run (in linear registration order) when a given label is run
///  - repeated calls with the same function type will reuse cached state, including for change detection
///
/// Any [`Commands`](crate::system::Commands) generated by these systems (but not other systems), will immediately be applied.
///
/// This type is stored as a [`Resource`](crate::system::Resource) on each [`World`], initialized by default.
/// However, it will likely be easier to use the corresponding methods on [`World`],
/// to avoid having to worry about split mutable borrows yourself.
///
/// # Limitations
///
///  - stored systems cannot be chained: they can neither have an [`In`](crate::system::In) nor return any values
///  - stored systems cannot recurse: they cannot run other systems via the [`SystemRegistry`] methods on `World` or `Commands`
///  - exclusive systems cannot be used
///
/// # Examples
///
/// You can run a single system directly on the World,
/// applying its effect and caching its state for the next time
/// you call this method (internally, this is based on [`SystemTypeIdLabel`]).
///
/// ```rust
/// use bevy_ecs::prelude::*;
///
/// let mut world = World::new();  
///
/// #[derive(Default, PartialEq, Debug)]
/// struct Counter(u8);
///
/// fn count_up(mut counter: ResMut<Counter>){
///     counter.0 += 1;
/// }
///
/// world.init_resource::<Counter>();
/// world.run_system(count_up);
///
/// assert_eq!(Counter(1), *world.resource());
/// ```
///
/// These systems immediately apply commands and cache state,
/// ensuring that change detection and [`Local`](crate::system::Local) variables work correctly.
///
/// ```rust
/// use bevy_ecs::prelude::*;
///
/// let mut world = World::new();
///
/// #[derive(Component)]
/// struct Marker;
///
/// fn spawn_7_entities(mut commands: Commands) {
///     for _ in 0..7 {
///         commands.spawn().insert(Marker);
///     }
/// }
///
/// fn assert_7_spawned(query: Query<(), Added<Marker>>){
///     let n_spawned = query.iter().count();
///     assert_eq!(n_spawned, 7);
/// }
///
/// world.run_system(spawn_7_entities);
/// world.run_system(assert_7_spawned);
/// ```
#[derive(Default)]
pub struct SystemRegistry {
    systems: Vec<StoredSystem>,
    // Stores the index of all systems that match the key's label
    labels: HashMap<Box<dyn SystemLabel>, Vec<usize>>,
}

struct StoredSystem {
    system: Box<dyn System<In = (), Out = ()>>,
}

impl SystemRegistry {
    /// Registers a system in the [`SystemRegistry`], so then it can be later run.
    ///
    /// This allows the system to be run by their [`SystemTypeIdLabel`] using the `run_systems_by_label` method.
    /// Repeatedly registering a system will have no effect.
    ///
    /// Ordinarily, systems are automatically registered when [`run_system`](SystemRegistry::run_system) is called.
    /// When this occurs, they will be registered using their [`SystemTypeIdLabel`].
    /// However, manual registration allows you to provide one or more labels for the system.
    /// This also allows you to register multiple distinct copies of the system under distinct labels.
    ///
    /// When [`run_systems_by_label`](SystemRegistry::run_systems_by_label) is called,
    /// all registered systems that match that label will be evaluated (in insertion order).
    ///
    /// To provide explicit label(s), use [`register_system_with_labels`](SystemRegistry::register_system_with_labels).
    #[inline]
    pub fn register_system<Params, S: IntoSystem<(), (), Params> + 'static>(
        &mut self,
        world: &mut World,
        system: S,
    ) {
        let automatic_system_label: SystemTypeIdLabel<S> = SystemTypeIdLabel::new();

        // This avoids nasty surprising behavior in case systems are registered twice
        if !self.is_label_registered(automatic_system_label) {
            let boxed_system: Box<dyn System<In = (), Out = ()>> =
                Box::new(IntoSystem::into_system(system));
            self.register_boxed_system_with_labels(
                world,
                boxed_system,
                vec![Box::new(automatic_system_label)],
            );
        } else {
            let type_name = std::any::type_name::<S>();
            warn!("A system of type {type_name} was registered more than once!");
        };
    }

    /// Register system a system with any number of [`SystemLabel`]s.
    ///
    /// This allows the system to be run whenever any of its labels are run using [`run_systems_by_label`](SystemRegistry::run_systems_by_label).
    ///
    /// # Warning
    ///
    /// Unlike the `register_system` method, duplicate systems may be added;
    /// each copy will be called seperately if they share a label.
    pub fn register_system_with_labels<
        Params,
        S: IntoSystem<(), (), Params> + 'static,
        LI: IntoIterator<Item = L>,
        L: SystemLabel,
    >(
        &mut self,
        world: &mut World,
        system: S,
        labels: LI,
    ) {
        let boxed_system: Box<dyn System<In = (), Out = ()>> =
            Box::new(IntoSystem::into_system(system));

        let collected_labels = labels
            .into_iter()
            .map(|label| {
                let boxed_label: Box<dyn SystemLabel> = Box::new(label);
                boxed_label
            })
            .collect();

        self.register_boxed_system_with_labels(world, boxed_system, collected_labels);
    }

    /// A more exacting version of [`register_system_with_labels`](Self::register_system_with_labels).
    ///
    /// Returns the index in the vector of systems that this new system is stored at.
    /// This is only useful for debugging as an external user of this method.
    ///
    /// This can be useful when you have a boxed system or boxed labels,
    /// as the corresponding traits are not implemented for boxed trait objects
    /// to avoid indefinite nesting.
    pub fn register_boxed_system_with_labels(
        &mut self,
        world: &mut World,
        mut boxed_system: Box<dyn System<In = (), Out = ()>>,
        labels: Vec<Box<dyn SystemLabel>>,
    ) -> usize {
        // Intialize the system's state
        boxed_system.initialize(world);

        let stored_system = StoredSystem {
            system: boxed_system,
        };

        // Add the system to the end of the vec
        let system_index = self.systems.len();
        self.systems.push(stored_system);

        // For each label that the system has
        for label in labels {
            let maybe_label_indexes = self.labels.get_mut(&label);

            // Add the index of the system in the vec to the lookup hashmap
            // under the corresponding label key
            if let Some(label_indexes) = maybe_label_indexes {
                label_indexes.push(system_index);
            } else {
                self.labels.insert(label, vec![system_index]);
            };
        }

        system_index
    }

    /// Runs the system at the supplied `index` a single time.
    #[inline]
    fn run_system_at_index(&mut self, world: &mut World, index: usize) {
        let stored_system = &mut self.systems[index];

        // Run the system
        stored_system.system.run((), world);
        // Apply any generated commands
        stored_system.system.apply_buffers(world);
    }

    /// Is at least one system in the [`SystemRegistry`] associated with the provided [`SystemLabel`]?
    #[inline]
    pub fn is_label_registered<L: SystemLabel>(&self, label: L) -> bool {
        let boxed_label: Box<dyn SystemLabel> = Box::new(label);
        self.labels.get(&boxed_label).is_some()
    }

    /// Returns the first matching index for systems with this label if any.
    #[inline]
    fn first_registered_index<L: SystemLabel>(&self, label: L) -> Option<usize> {
        let boxed_label: Box<dyn SystemLabel> = Box::new(label);
        let vec_of_indexes = self.labels.get(&boxed_label)?;
        vec_of_indexes.iter().next().copied()
    }

    /// Runs the set of systems corresponding to the provided [`SystemLabel`] on the [`World`] a single time.
    ///
    /// Systems will be run sequentially in registration order if more than one registered system matches the provided label.
    pub fn run_systems_by_label<L: SystemLabel>(
        &mut self,
        world: &mut World,
        label: L,
    ) -> Result<(), SystemRegistryError> {
        self.run_callback(
            world,
            Callback {
                label: label.dyn_clone(),
            },
        )
    }

    /// Run the systems corresponding to the label stored in the provided [`Callback`]
    ///
    /// Systems must be registered before they can be run by their label,
    /// including via this method.
    ///
    /// Systems will be run sequentially in registration order if more than one registered system matches the provided label.
    #[inline]
    pub fn run_callback(
        &mut self,
        world: &mut World,
        callback: Callback,
    ) -> Result<(), SystemRegistryError> {
        let boxed_label = callback.label;

        match self.labels.get(&boxed_label) {
            Some(matching_indexes) => {
                // Loop over the system in registration order
                for index in matching_indexes.clone() {
                    self.run_system_at_index(world, index);
                }

                Ok(())
            }
            None => Err(SystemRegistryError::LabelNotFound(boxed_label)),
        }
    }

    /// Runs the supplied system on the [`World`] a single time.
    ///
    /// System state will be reused between runs, ensuring that [`Local`](crate::system::Local) variables and change detection works correctly.
    /// If, via manual system registration, you have somehow managed to insert more than one system with the same [`SystemTypeIdLabel`],
    /// only the first will be run.
    pub fn run_system<Params, S: IntoSystem<(), (), Params> + 'static>(
        &mut self,
        world: &mut World,
        system: S,
    ) {
        let automatic_system_label: SystemTypeIdLabel<S> = SystemTypeIdLabel::new();
        let index = if self.is_label_registered(automatic_system_label) {
            self.first_registered_index(automatic_system_label).unwrap()
        } else {
            let boxed_system: Box<dyn System<In = (), Out = ()>> =
                Box::new(IntoSystem::into_system(system));
            let labels = boxed_system.default_labels();
            self.register_boxed_system_with_labels(world, boxed_system, labels)
        };

        self.run_system_at_index(world, index);
    }
}

impl World {
    /// Registers the supplied system in the [`SystemRegistry`] resource.
    ///
    /// Calls the method of the same name on [`SystemRegistry`].
    #[inline]
    pub fn register_system<Params, S: IntoSystem<(), (), Params> + 'static>(&mut self, system: S) {
        self.resource_scope(|world, mut registry: Mut<SystemRegistry>| {
            registry.register_system(world, system);
        });
    }

    /// Register system a system with any number of [`SystemLabel`]s.
    ///
    /// Calls the method of the same name on [`SystemRegistry`].
    pub fn register_system_with_labels<
        Params,
        S: IntoSystem<(), (), Params> + 'static,
        LI: IntoIterator<Item = L>,
        L: SystemLabel,
    >(
        &mut self,
        system: S,
        labels: LI,
    ) {
        self.resource_scope(|world, mut registry: Mut<SystemRegistry>| {
            registry.register_system_with_labels(world, system, labels);
        });
    }

    /// Runs the supplied system on the [`World`] a single time.
    ///
    /// Calls the method of the same name on [`SystemRegistry`].
    #[inline]
    pub fn run_system<Params, S: IntoSystem<(), (), Params> + 'static>(&mut self, system: S) {
        self.resource_scope(|world, mut registry: Mut<SystemRegistry>| {
            registry.run_system(world, system);
        });
    }

    /// Runs the systems corresponding to the supplied [`SystemLabel`] on the [`World`] a single time.
    ///
    /// Calls the method of the same name on [`SystemRegistry`].
    #[inline]
    pub fn run_systems_by_label<L: SystemLabel>(
        &mut self,
        label: L,
    ) -> Result<(), SystemRegistryError> {
        self.resource_scope(|world, mut registry: Mut<SystemRegistry>| {
            registry.run_systems_by_label(world, label)
        })
    }

    /// Run the systems corresponding to the label stored in the provided [`Callback`]
    ///
    /// Calls the method of the same name on [`SystemRegistry`].
    #[inline]
    pub fn run_callback(&mut self, callback: Callback) -> Result<(), SystemRegistryError> {
        self.resource_scope(|world, mut registry: Mut<SystemRegistry>| {
            registry.run_callback(world, callback)
        })
    }
}

/// The [`Command`] type for [`SystemRegistry::run_system`]
#[derive(Debug, Clone)]
pub struct RunSystemCommand<
    Params: Send + Sync + 'static,
    S: IntoSystem<(), (), Params> + Send + Sync + 'static,
> {
    _phantom_params: PhantomData<Params>,
    system: S,
}

impl<Params: Send + Sync + 'static, S: IntoSystem<(), (), Params> + Send + Sync + 'static>
    RunSystemCommand<Params, S>
{
    /// Creates a new [`Command`] struct, which can be added to [`Commands`](crate::system::Commands)
    #[inline]
    #[must_use]
    pub fn new(system: S) -> Self {
        Self {
            _phantom_params: PhantomData::default(),
            system,
        }
    }
}

impl<Params: Send + Sync + 'static, S: IntoSystem<(), (), Params> + Send + Sync + 'static> Command
    for RunSystemCommand<Params, S>
{
    #[inline]
    fn write(self, world: &mut World) {
        world.run_system(self.system);
    }
}

/// The [`Command`] type for [`SystemRegistry::run_systems_by_label`]
#[derive(Debug, Clone)]
pub struct RunSystemsByLabelCommand {
    pub callback: Callback,
}

impl Command for RunSystemsByLabelCommand {
    #[inline]
    fn write(self, world: &mut World) {
        world.resource_scope(|world, mut registry: Mut<SystemRegistry>| {
            registry
                .run_callback(world, self.callback)
                // Ideally this error should be handled more gracefully,
                // but that's blocked on a full error handling solution for commands
                .unwrap();
        });
    }
}

/// A struct that stores a boxed [`SystemLabel`], used to cause a [`SystemRegistry`] to run systems.
///
/// This might be stored as a component, used as an event, or arranged in a queue stored in a resource.
/// Unless you need to inspect the list of events or add additional information,
/// prefer the simpler `commands.run_system` over storing callbacks as events,
///
/// When working with callbacks, consider your architecture carefully.
/// Callbacks are typically harder to reason about and debug than scheduled systems,
/// and it's easy to get into a tangled mess if you don't consider the system as a whole before starting.
///
/// Systems must be registered via the `register_system` methods on [`SystemRegistry`], [`World`] or `App`
/// before they can be run by their label using a callback.
#[derive(Debug, Component, Clone, Eq)]
pub struct Callback {
    /// The label of the system(s) to be run.
    ///
    /// By default, this is set to the [`SystemTypeIdLabel`]
    /// of the system passed in via [`Callback::new()`].
    pub label: Box<dyn SystemLabel>,
}

impl Callback {
    /// Creates a new callback from a function that can be used as a system.
    ///
    /// Remember that you must register your systems with the `App` / [`World`] before they can be run as callbacks!
    pub fn new<S: IntoSystemDescriptor<Params> + 'static, Params>(_system: S) -> Self {
        Callback {
            label: Box::new(SystemTypeIdLabel::<S>::new()),
        }
    }
}

impl PartialEq for Callback {
    fn eq(&self, other: &Self) -> bool {
        self.label.dyn_eq(other.label.as_dyn_eq())
    }
}

impl Hash for Callback {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.label.dyn_hash(state);
    }
}

/// An operation on a [`SystemRegistry`] failed
#[derive(Debug)]
pub enum SystemRegistryError {
    /// A system was run by label, but no system with that label was found.
    ///
    /// Did you forget to register it?
    LabelNotFound(Box<dyn SystemLabel>),
}

mod tests {
    use crate::prelude::*;

    #[derive(Default, PartialEq, Debug)]
    struct Counter(u8);

    #[allow(dead_code)]
    fn count_up(mut counter: ResMut<Counter>) {
        counter.0 += 1;
    }

    #[test]
    fn run_system() {
        let mut world = World::new();
        world.init_resource::<Counter>();
        assert_eq!(*world.resource::<Counter>(), Counter(0));
        world.run_system(count_up);
        assert_eq!(*world.resource::<Counter>(), Counter(1));
    }

    #[test]
    /// We need to ensure that the system registry is accessible
    /// even after being used once.
    fn run_two_systems() {
        let mut world = World::new();
        world.init_resource::<Counter>();
        assert_eq!(*world.resource::<Counter>(), Counter(0));
        world.run_system(count_up);
        assert_eq!(*world.resource::<Counter>(), Counter(1));
        world.run_system(count_up);
        assert_eq!(*world.resource::<Counter>(), Counter(2));
    }

    #[test]
    fn run_system_by_label() {
        let mut world = World::new();
        world.init_resource::<Counter>();
        assert_eq!(*world.resource::<Counter>(), Counter(0));
        world.register_system_with_labels(count_up, ["count"]);
        world.register_system_with_labels(count_up, ["count"]);
        world.run_systems_by_label("count").unwrap();
        // All systems matching the label will be run.
        assert_eq!(*world.resource::<Counter>(), Counter(2));
    }

    #[allow(dead_code)]
    fn spawn_entity(mut commands: Commands) {
        commands.spawn();
    }

    #[test]
    fn command_processing() {
        let mut world = World::new();
        world.init_resource::<Counter>();
        assert_eq!(world.entities.len(), 0);
        world.run_system(spawn_entity);
        assert_eq!(world.entities.len(), 1);
    }

    #[test]
    fn non_send_resources() {
        fn non_send_count_down(mut ns: NonSendMut<Counter>) {
            ns.0 -= 1;
        }

        let mut world = World::new();
        world.insert_non_send_resource(Counter(10));
        assert_eq!(*world.non_send_resource::<Counter>(), Counter(10));
        world.run_system(non_send_count_down);
        assert_eq!(*world.non_send_resource::<Counter>(), Counter(9));
    }

    #[test]
    fn change_detection() {
        #[derive(Default)]
        struct ChangeDetector;

        #[allow(dead_code)]
        fn count_up_iff_changed(
            mut counter: ResMut<Counter>,
            change_detector: ResMut<ChangeDetector>,
        ) {
            if change_detector.is_changed() {
                counter.0 += 1;
            }
        }

        let mut world = World::new();
        world.init_resource::<ChangeDetector>();
        world.init_resource::<Counter>();
        assert_eq!(*world.resource::<Counter>(), Counter(0));
        // Resources are changed when they are first added.
        world.run_system(count_up_iff_changed);
        assert_eq!(*world.resource::<Counter>(), Counter(1));
        // Nothing changed
        world.run_system(count_up_iff_changed);
        assert_eq!(*world.resource::<Counter>(), Counter(1));
        // Making a change
        world.resource_mut::<ChangeDetector>().set_changed();
        world.run_system(count_up_iff_changed);
        assert_eq!(*world.resource::<Counter>(), Counter(2));
    }

    #[test]
    fn local_variables() {
        // The `Local` begins at the default value of 0
        fn doubling(mut last_counter: Local<Counter>, mut counter: ResMut<Counter>) {
            counter.0 += last_counter.0;
            last_counter.0 = counter.0;
        }

        let mut world = World::new();
        world.insert_resource(Counter(1));
        assert_eq!(*world.resource::<Counter>(), Counter(1));
        world.run_system(doubling);
        assert_eq!(*world.resource::<Counter>(), Counter(1));
        world.run_system(doubling);
        assert_eq!(*world.resource::<Counter>(), Counter(2));
        world.run_system(doubling);
        assert_eq!(*world.resource::<Counter>(), Counter(4));
        world.run_system(doubling);
        assert_eq!(*world.resource::<Counter>(), Counter(8));
    }

    #[test]
    // This is a known limitation;
    // if this test passes the docs must be updated
    // to reflect the ability to chain run_system commands
    #[should_panic]
    fn system_recursion() {
        fn count_to_ten(mut counter: ResMut<Counter>, mut commands: Commands) {
            counter.0 += 1;
            if counter.0 < 10 {
                commands.run_system(count_to_ten);
            }
        }

        let mut world = World::new();
        world.init_resource::<Counter>();
        assert_eq!(*world.resource::<Counter>(), Counter(0));
        world.run_system(count_to_ten);
        assert_eq!(*world.resource::<Counter>(), Counter(10));
    }
}
