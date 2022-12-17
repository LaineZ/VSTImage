pub mod instance;

use std::{
    io::{Cursor, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use image::{
    imageops::{crop_imm, replace},
    io::Reader as ImageReader,
    RgbaImage,
};
use log::debug;

use crate::{
    image_utils::{self, SplittedImage, IMAGE_SPLIT_H, IMAGE_SPLIT_W},
    models::area::Area,
    processing::{rgba_to_sample, sample_to_rgba},
};
use anyhow::Result;

use vst::{
    host::{Host, HostBuffer, PluginInstance, PluginLoader},
    prelude::Plugin,
};

use self::instance::PluginRackInstance;

pub struct PluginHost;

pub struct PluginRack {
    pub host: Arc<Mutex<PluginHost>>,
    pub plugins: Vec<PluginRackInstance>,
    pub images: Vec<SplittedImage>,
    /// Current tile processing position
    position: usize,
    /// Total processing tiles
    total: usize,
    finished: bool,
}

impl Host for PluginHost {
    fn automate(&self, index: i32, value: f32) {
        debug!("Parameter {} had its value changed to {}", index, value);
    }

    fn process_events(&self, events: &vst::api::Events) {
        debug!("Plugin called the {:?} event", events.events);
    }

    fn update_display(&self) {
        self.idle();
    }
}

impl PluginRack {
    pub fn new() -> Self {
        let host = Arc::new(Mutex::new(PluginHost));
        Self {
            host,
            plugins: Vec::new(),
            images: Vec::new(),
            position: 0,
            total: 0,
            finished: true,
        }
    }

    pub fn undo(&mut self) {
        if self.images.len() > 1 {
            self.images.remove(self.images.len() - 1);
            self.images.last_mut().unwrap().request_all_update();
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn load_plugin(&mut self, file: PathBuf) -> anyhow::Result<()> {
        let mut loader = PluginLoader::load(&file, Arc::clone(&self.host))?;
        let instance = loader.instance()?;
        self.insert_plugin(file, instance);
        Ok(())
    }

    pub fn compute_complete_percentage(&self) -> usize {
        (self.position as f32 / self.total as f32 * 100.0) as usize
    }

    pub fn get_processed_position(&self) -> usize {
        self.position
    }

    pub fn get_processing_size(&self) -> usize {
        self.total
    }

    pub fn load_uninitialzed_plugins(&mut self) -> anyhow::Result<()> {
        for plugin in &mut self.plugins {
            if let Ok(mut loader) = PluginLoader::load(&plugin.get_path(), Arc::clone(&self.host)) {
                if let Ok(instance) = loader.instance() {
                    plugin.instance = Some(instance);
                    plugin.initialize()?;
                }
            }
        }

        Ok(())
    }

    pub fn load_image<P: AsRef<std::path::Path>>(&mut self, file: P) -> anyhow::Result<()> {
        self.images.clear();
        let img = ImageReader::open(file)?.decode()?;

        let split = image_utils::SplittedImage::new(&mut img.into_rgba8());
        self.images.push(split);
        Ok(())
    }

    pub fn load_image_data(&mut self, file: &[u8]) -> anyhow::Result<()> {
        self.images.clear();
        let img = ImageReader::new(Cursor::new(file))
            .with_guessed_format()?
            .decode()?;

        let split = image_utils::SplittedImage::new(&mut img.into_rgba8());
        self.images.push(split);
        Ok(())
    }

    pub fn load_image_rgba(&mut self, image: &mut RgbaImage) {
        self.images.clear();
        self.images.push(image_utils::SplittedImage::new(image));
    }

    pub fn save_image<P: AsRef<std::path::Path>>(&self, file: P) -> Result<(), image::ImageError> {
        let img = self.images.last().unwrap().join_image();
        img.save(file)
    }

    pub fn save_project(&mut self, file: std::path::PathBuf) -> anyhow::Result<()> {
        for plugin in &mut self.plugins {
            plugin.save_block();
        }

        let file = std::fs::File::create(&file).unwrap();

        let mut zip = zip::ZipWriter::new(file);

        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Zstd);

        zip.start_file("project.json", options)?;
        let j = serde_json::to_string(&self.plugins)?;
        zip.write_all(j.as_bytes())?;

        zip.start_file("image.png", options)?;
        let mut bytes: Vec<u8> = Vec::new();

        let img = self.images.last().unwrap().join_image();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageOutputFormat::Png)?;
        zip.write_all(&bytes)?;

        zip.finish()?;
        Ok(())
    }

    fn insert_plugin(&mut self, file: PathBuf, instance: PluginInstance) {
        self.plugins.push(PluginRackInstance::new(file, instance));
        self.plugins.last_mut().unwrap().initialize().unwrap();
    }

    pub fn remove_plugin(&mut self, id: usize) {
        debug!("Removing: {}", id);
        if let Some(instance) = self.plugins[id].instance.as_mut() {
            instance.suspend();
        }
        self.plugins.remove(id);
    }

    pub fn start_process(&mut self) {
        if self.plugins.is_empty() || self.images.is_empty() {
            return;
        }

        let img = self.images.last().unwrap().clone();

        if self.images.len() >= 2 {
            self.images.remove(1);
        }

        self.images.push(img.clone());
        self.finished = false;
        self.position = 0;
        self.total = self.images.last().unwrap().splits.len() - 1;

        for plugin in &mut self.plugins {
            let instance = plugin.instance.as_mut();

            if instance.is_none() {
                continue;
            }

            let instance = instance.unwrap();

            instance.suspend();
            instance.set_block_size(256 * 256);
            instance.resume();
        }
    }

    pub fn stop_process(&mut self) {
        self.images.remove(self.images.len() - 1);
        self.finished = true;
        self.position = 0;
        self.total = 0;
        self.images.last_mut().unwrap().request_all_update();
    }

    /// Lazy iterative processing of VST effects (should called in a loop)
    pub fn process_next(&mut self) {
        if self.plugins.is_empty() {
            self.finished = true;
            return;
        }

        if self.finished || self.plugins.is_empty() {
            return;
        }

        //let full_process_time = std::time::Instant::now();

        let last_image = &mut self.images.last_mut().unwrap().splits;

        for plugin in &mut self.plugins {
            let instance = plugin.instance.as_mut();

            if instance.is_none() {
                continue;
            }

            let instance = instance.unwrap();
            let start = std::time::Instant::now();
            let input_count = instance.get_info().inputs as usize;
            let output_count = instance.get_info().outputs as usize;

            if plugin.bypass || input_count == 0 {
                continue;
            }
            debug!("i: {} o: {}", input_count, output_count);
            // zeroing buffers
            let mut buf: HostBuffer<f32> = HostBuffer::new(input_count, output_count);
            let mut inputs: Vec<Vec<f32>> = vec![vec![0.0]; input_count];
            let mut outputs = vec![vec![0.0]; output_count];

            for sample in last_image[self.position].data.pixels() {
                for i in 0..input_count {
                    inputs[i].push(rgba_to_sample(plugin.input_channel, sample))
                }

                for i in 0..output_count {
                    outputs[i].push(0.0);
                }
            }

            let mut audio_buffer = buf.bind(&inputs, &mut outputs);

            debug!("Mapping took: {} ms", start.elapsed().as_millis());

            let start = std::time::Instant::now();
            debug!("processing");
            instance.start_process();
            instance.process(&mut audio_buffer);

            debug!("VST Processing took: {} ms", start.elapsed().as_millis());
            let start = std::time::Instant::now();
            for (pixel, sample) in last_image[self.position]
                .data
                .pixels_mut()
                .zip(&outputs[plugin.output_channel])
            {
                sample_to_rgba(*sample, plugin.wet, pixel, plugin.input_channel);
            }
            debug!("Image return took: {} ms", start.elapsed().as_millis());
        }

        last_image[self.position].needs_update = true;
        if self.total <= self.position {
            self.finished = true;
            for plugin in &mut self.plugins {
                if let Some(instance) = plugin.instance.as_mut() {
                    instance.stop_process();
                    instance.suspend();
                }
            }
            debug!("finished")
        } else {
            self.position += 1;
        }

        debug!("{}/{}", self.position, self.total);
    }

    pub fn process_area(&mut self, area: Area) {
        for plugin in &mut self.plugins {
            let instance = plugin.instance.as_mut();

            if instance.is_none() || plugin.bypass {
                continue;
            }
            let instance = instance.unwrap();
            let input_count = instance.get_info().inputs as usize;
            let output_count = instance.get_info().outputs as usize;

            if input_count == 0 {
                continue;
            }

            let mut buf: HostBuffer<f32> = HostBuffer::new(input_count, output_count);
            let mut inputs: Vec<Vec<f32>> = vec![vec![0.0]; input_count];
            let mut outputs = vec![vec![0.0]; output_count];
            let last_image = self.images.last_mut().unwrap();

            let chunk_x = area.x / IMAGE_SPLIT_W;
            let chunk_y = area.y / IMAGE_SPLIT_H;

            let orig_width_tiles = last_image.origianl_dimensions().width / IMAGE_SPLIT_W;

            let mut current_split =
                &mut last_image.splits[(orig_width_tiles * chunk_y + chunk_x) as usize];

            let x_f = area.x % current_split.location().width;
            let y_f = area.y % current_split.location().height;

            let crop = crop_imm(&current_split.data, x_f, y_f, area.width, area.height);
            let mut crop_img = crop.to_image();

            debug!("{}x{} w: {} h: {}", x_f, y_f, crop_img.width(), crop_img.height());

            for sample in crop_img.pixels() {
                for i in 0..input_count {
                    inputs[i].push(rgba_to_sample(plugin.input_channel, sample))
                }

                for i in 0..output_count {
                    outputs[i].push(0.0);
                }
            }

            let mut audio_buffer = buf.bind(&inputs, &mut outputs);

            instance.suspend();
            instance.set_block_size(area.area() as i64);
            instance.resume();
            instance.start_process();
            instance.process(&mut audio_buffer);
            instance.stop_process();
            instance.suspend();

            for (pixel, sample) in crop_img.pixels_mut().zip(&outputs[plugin.output_channel]) {
                sample_to_rgba(*sample, plugin.wet, pixel, plugin.input_channel);
            }

            replace(&mut current_split.data, &crop_img, x_f as i64, y_f as i64);

            current_split.needs_update = true;
        }
    }
}
