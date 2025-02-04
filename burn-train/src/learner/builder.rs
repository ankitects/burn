use super::log::update_log_file;
use super::Learner;
use crate::checkpoint::{AsyncCheckpointer, Checkpointer, FileCheckpointer};
use crate::logger::{FileMetricLogger, MetricLogger};
use crate::metric::dashboard::cli::CLIDashboardRenderer;
use crate::metric::dashboard::Dashboard;
use crate::metric::{Adaptor, Metric, Numeric};
use crate::AsyncTrainerCallback;
use burn_core::lr_scheduler::LRScheduler;
use burn_core::module::ADModule;
use burn_core::optim::Optimizer;
use burn_core::record::FileRecorder;
use burn_core::tensor::backend::ADBackend;

use std::sync::Arc;

/// Struct to configure and create a [learner](Learner).
pub struct LearnerBuilder<B, T, V, M, O, S>
where
    T: Send + Sync + 'static,
    V: Send + Sync + 'static,
    B: ADBackend,
    M: ADModule<B>,
    O: Optimizer<M, B>,
    S: LRScheduler,
{
    dashboard: Dashboard<T, V>,
    checkpointer_model: Option<Arc<dyn Checkpointer<M::Record> + Send + Sync>>,
    checkpointer_optimizer: Option<Arc<dyn Checkpointer<O::Record> + Send + Sync>>,
    checkpointer_scheduler: Option<Arc<dyn Checkpointer<S::Record> + Send + Sync>>,
    num_epochs: usize,
    checkpoint: Option<usize>,
    directory: String,
    grad_accumulation: Option<usize>,
    devices: Vec<B::Device>,
}

impl<B, T, V, Model, Optim, LR> LearnerBuilder<B, T, V, Model, Optim, LR>
where
    T: Send + Sync + 'static,
    V: Send + Sync + 'static,
    B: ADBackend,
    Model: ADModule<B>,
    Optim: Optimizer<Model, B>,
    LR: LRScheduler,
{
    /// Creates a new learner builder.
    ///
    /// # Arguments
    ///
    /// * `directory` - The directory to save the checkpoints.
    pub fn new(directory: &str) -> Self {
        let renderer = Box::new(CLIDashboardRenderer::new());
        let logger_train = Box::new(FileMetricLogger::new(format!("{directory}/train").as_str()));
        let logger_valid = Box::new(FileMetricLogger::new(format!("{directory}/valid").as_str()));

        Self {
            dashboard: Dashboard::new(renderer, logger_train, logger_valid),
            num_epochs: 1,
            checkpoint: None,
            checkpointer_model: None,
            checkpointer_optimizer: None,
            checkpointer_scheduler: None,
            directory: directory.to_string(),
            grad_accumulation: None,
            devices: vec![B::Device::default()],
        }
    }

    /// Replace the default metric loggers with the provided ones.
    ///
    /// # Arguments
    ///
    /// * `logger_train` - The training logger.
    /// * `logger_valid` - The validation logger.
    pub fn metric_loggers<MT, MV>(mut self, logger_train: MT, logger_valid: MV) -> Self
    where
        MT: MetricLogger + 'static,
        MV: MetricLogger + 'static,
    {
        self.dashboard
            .replace_loggers(Box::new(logger_train), Box::new(logger_valid));
        self
    }

    /// Register a training metric.
    pub fn metric_train<M: Metric + 'static>(mut self, metric: M) -> Self
    where
        T: Adaptor<M::Input>,
    {
        self.dashboard.register_train(metric);
        self
    }

    /// Register a validation metric.
    pub fn metric_valid<M: Metric + 'static>(mut self, metric: M) -> Self
    where
        V: Adaptor<M::Input>,
    {
        self.dashboard.register_valid(metric);
        self
    }

    /// Enable gradients accumulation.
    ///
    /// # Notes
    ///
    /// When you enable gradients accumulation, the gradients object used by the optimizer will be
    /// the sum of all gradients generated by each backward pass. It might be a good idea to
    /// reduce the learning to compensate.
    ///
    /// The effect is similar to increasing the `batch size` and the `learning rate` by the `accumulation`
    /// amount.
    pub fn grads_accumulation(mut self, accumulation: usize) -> Self {
        self.grad_accumulation = Some(accumulation);
        self
    }

    /// Register a training metric and displays it on a plot.
    ///
    /// # Notes
    ///
    /// Only [numeric](Numeric) metric can be displayed on a plot.
    /// If the same metric is also registered for the [validation split](Self::metric_valid_plot),
    /// the same graph will be used for both.
    pub fn metric_train_plot<M>(mut self, metric: M) -> Self
    where
        M: Metric + Numeric + 'static,
        T: Adaptor<M::Input>,
    {
        self.dashboard.register_train_plot(metric);
        self
    }

    /// Register a validation metric and displays it on a plot.
    ///
    /// # Notes
    ///
    /// Only [numeric](Numeric) metric can be displayed on a plot.
    /// If the same metric is also registered for the [training split](Self::metric_train_plot),
    /// the same graph will be used for both.
    pub fn metric_valid_plot<M: Metric + Numeric + 'static>(mut self, metric: M) -> Self
    where
        V: Adaptor<M::Input>,
    {
        self.dashboard.register_valid_plot(metric);
        self
    }

    /// The number of epochs the training should last.
    pub fn num_epochs(mut self, num_epochs: usize) -> Self {
        self.num_epochs = num_epochs;
        self
    }

    /// Run the training loop on multiple devices.
    pub fn devices(mut self, devices: Vec<B::Device>) -> Self {
        self.devices = devices;
        self
    }

    /// The epoch from which the training must resume.
    pub fn checkpoint(mut self, checkpoint: usize) -> Self {
        self.checkpoint = Some(checkpoint);
        self
    }

    /// Register a checkpointer that will save the [optimizer](Optimizer) and the
    /// [model](ADModule).
    ///
    /// The number of checkpoints to be keep should be set to a minimum of two to be safe, since
    /// they are saved and deleted asynchronously and a crash during training might make a
    /// checkpoint non-usable.
    pub fn with_file_checkpointer<FR>(mut self, num_keep: usize, recorder: FR) -> Self
    where
        FR: FileRecorder + 'static,
    {
        self.checkpointer_model = Some(Arc::new(FileCheckpointer::new(
            recorder.clone(),
            format!("{}/checkpoint", self.directory).as_str(),
            "model",
            num_keep,
        )));
        self.checkpointer_optimizer = Some(Arc::new(FileCheckpointer::new(
            recorder.clone(),
            format!("{}/checkpoint", self.directory).as_str(),
            "optim",
            num_keep,
        )));
        self.checkpointer_scheduler = Some(Arc::new(FileCheckpointer::new(
            recorder,
            format!("{}/checkpoint", self.directory).as_str(),
            "scheduler",
            num_keep,
        )));
        self
    }

    /// Create the [learner](Learner) from a [model](ADModule) and an [optimizer](Optimizer).
    /// The [learning rate scheduler](LRScheduler) can also be a simple
    /// [learning rate](burn_core::LearningRate).
    pub fn build(
        self,
        model: Model,
        optim: Optim,
        lr_scheduler: LR,
    ) -> Learner<B, Model, Optim, LR, T, V>
    where
        Model::Record: 'static,
        Optim::Record: 'static,
        LR::Record: 'static,
    {
        self.init_logger();
        let callback = Box::new(self.dashboard);
        let callback = Box::new(AsyncTrainerCallback::new(callback));

        let checkpointer_optimizer = match self.checkpointer_optimizer {
            Some(checkpointer) => {
                let checkpointer: Box<dyn Checkpointer<Optim::Record>> =
                    Box::new(AsyncCheckpointer::new(checkpointer));
                Some(checkpointer)
            }
            None => None,
        };
        let checkpointer_model = match self.checkpointer_model {
            Some(checkpointer) => {
                let checkpointer: Box<dyn Checkpointer<Model::Record>> =
                    Box::new(AsyncCheckpointer::new(checkpointer));
                Some(checkpointer)
            }
            None => None,
        };
        let checkpointer_scheduler = match self.checkpointer_scheduler {
            Some(checkpointer) => {
                let checkpointer: Box<dyn Checkpointer<LR::Record>> =
                    Box::new(AsyncCheckpointer::new(checkpointer));
                Some(checkpointer)
            }
            None => None,
        };

        Learner {
            model,
            optim,
            lr_scheduler,
            num_epochs: self.num_epochs,
            callback,
            checkpoint: self.checkpoint,
            checkpointer_model,
            checkpointer_optimizer,
            checkpointer_scheduler,
            grad_accumulation: self.grad_accumulation,
            devices: self.devices,
        }
    }

    fn init_logger(&self) {
        let file_path = format!("{}/experiment.log", self.directory);
        update_log_file(file_path.as_str());
    }
}
