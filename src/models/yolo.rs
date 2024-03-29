use anyhow::Result;
use clap::ValueEnum;
use image::{DynamicImage, ImageBuffer};
use ndarray::{s, Array, Axis, IxDyn};
use regex::Regex;

use crate::{
    non_max_suppression, ops, Annotator, Bbox, DynConf, Embedding, Keypoint, MinOptMax, Options,
    OrtEngine, Point, Rect, Results,
};

const CXYWH_OFFSET: usize = 4;
const KPT_STEP: usize = 3;

#[derive(Debug, Clone, ValueEnum)]
enum YOLOTask {
    Classify,
    Detect,
    Pose,
    Segment,
    Obb, // TODO
}

#[derive(Debug)]
pub struct YOLO {
    engine: OrtEngine,
    nc: usize,
    nk: usize,
    nm: usize,
    height: MinOptMax,
    width: MinOptMax,
    batch: MinOptMax,
    task: YOLOTask,
    confs: DynConf,
    kconfs: DynConf,
    iou: f32,
    saveout: Option<String>,
    annotator: Annotator,
    names: Option<Vec<String>>,
    apply_nms: bool,
    anchors_first: bool,
}

impl YOLO {
    pub fn new(options: &Options) -> Result<Self> {
        let engine = OrtEngine::new(options)?;
        let (batch, height, width) = (
            engine.batch().to_owned(),
            engine.height().to_owned(),
            engine.width().to_owned(),
        );
        let task = match engine
            .try_fetch("task")
            .unwrap_or("detect".to_string())
            .as_str()
        {
            "classify" => YOLOTask::Classify,
            "detect" => YOLOTask::Detect,
            "pose" => YOLOTask::Pose,
            "segment" => YOLOTask::Segment,
            x => todo!("{:?} is not supported for now!", x),
        };

        // try from custom class names, and then model metadata
        let mut names = options.names.to_owned().or(Self::fetch_names(&engine));
        let nc = match options.nc {
            Some(nc) => {
                match &names {
                    None => names = Some((0..nc).map(|x| x.to_string()).collect::<Vec<String>>()),
                    Some(names) => {
                        assert_eq!(
                            nc,
                            names.len(),
                            "the length of `nc` and `class names` is not equal."
                        );
                    }
                }
                nc
            }
            None => match &names {
                Some(names) => names.len(),
                None => panic!(
                    "Can not parse model without `nc` and `class names`. Try to make it explicit."
                ),
            },
        };

        // try from model metadata
        let nk = engine
            .try_fetch("kpt_shape")
            .map(|kpt_string| {
                let re = Regex::new(r"([0-9]+), ([0-9]+)").unwrap();
                let caps = re.captures(&kpt_string).unwrap();
                caps.get(1).unwrap().as_str().parse::<usize>().unwrap()
            })
            .unwrap_or(0_usize);
        let nm = if let YOLOTask::Segment = task {
            engine.oshapes()[1][1] as usize
        } else {
            0_usize
        };
        let confs = DynConf::new(&options.confs, nc);
        let kconfs = DynConf::new(&options.kconfs, nk);
        let mut annotator = Annotator::default();
        if let Some(skeletons) = &options.skeletons {
            annotator = annotator.with_skeletons(skeletons);
        }
        let saveout = options.saveout.to_owned();
        engine.dry_run()?;

        Ok(Self {
            engine,
            confs,
            kconfs,
            iou: options.iou,
            apply_nms: options.apply_nms,
            nc,
            nk,
            nm,
            height,
            width,
            batch,
            task,
            saveout,
            annotator,
            names,
            anchors_first: options.anchors_first,
        })
    }

    // pub fn run_with_dl(&mut self, dl: &Dataloader) -> Result<Vec<Results>> {
    //     for (images, paths) in dataloader {
    //         self.run(&images)
    //     }
    //     Ok(())
    // }

    pub fn run(&mut self, xs: &[DynamicImage]) -> Result<Vec<Results>> {
        let xs_ = ops::letterbox(xs, self.height() as u32, self.width() as u32)?;
        let ys = self.engine.run(&[xs_])?;
        let ys = self.postprocess(ys, xs)?;
        match &self.saveout {
            None => println!("{ys:?}"),
            Some(saveout) => {
                for (img0, y) in xs.iter().zip(ys.iter()) {
                    let mut img = img0.to_rgb8();
                    self.annotator.plot(&mut img, y);
                    self.annotator.save(&img, saveout);
                }
            }
        }
        Ok(ys)
    }

    pub fn postprocess(
        &self,
        xs: Vec<Array<f32, IxDyn>>,
        xs0: &[DynamicImage],
    ) -> Result<Vec<Results>> {
        if let YOLOTask::Classify = self.task {
            let mut ys = Vec::new();
            for batch in xs[0].axis_iter(Axis(0)) {
                ys.push(Results::new(
                    Some(Embedding::new(batch.into_owned(), self.names.to_owned())),
                    None,
                    None,
                    None,
                ));
            }
            Ok(ys)
        } else {
            let (preds, protos) = if xs.len() == 2 {
                if xs[0].ndim() == 3 {
                    (&xs[0], Some(&xs[1]))
                } else {
                    (&xs[1], Some(&xs[0]))
                }
            } else {
                (&xs[0], None)
            };

            let mut ys = Vec::new();
            for (idx, anchor) in preds.axis_iter(Axis(0)).enumerate() {
                // [b, 4 + nc + nm, na]
                // input image
                let width_original = xs0[idx].width() as f32;
                let height_original = xs0[idx].height() as f32;
                let ratio = (self.width() as f32 / width_original)
                    .min(self.height() as f32 / height_original);

                #[allow(clippy::type_complexity)]
                let mut data: Vec<(Bbox, Option<Vec<Keypoint>>, Option<Vec<f32>>)> = Vec::new();
                for pred in anchor.axis_iter(if self.anchors_first { Axis(0) } else { Axis(1) }) {
                    // split preds for different tasks
                    let bbox = pred.slice(s![0..CXYWH_OFFSET]);
                    let clss = pred.slice(s![CXYWH_OFFSET..CXYWH_OFFSET + self.nc]);
                    let kpts = {
                        if let YOLOTask::Pose = self.task {
                            Some(pred.slice(s![pred.len() - KPT_STEP * self.nk..]))
                        } else {
                            None
                        }
                    };
                    let coefs = {
                        if let YOLOTask::Segment = self.task {
                            Some(pred.slice(s![pred.len() - self.nm..]).to_vec())
                        } else {
                            None
                        }
                    };

                    // confidence and index
                    let (id, &confidence) = clss
                        .into_iter()
                        .enumerate()
                        .reduce(|max, x| if x.1 > max.1 { x } else { max })
                        .unwrap();

                    // confidence filter
                    if confidence < self.confs[id] {
                        continue;
                    }

                    // bbox re-scale
                    let cx = bbox[0] / ratio;
                    let cy = bbox[1] / ratio;
                    let w = bbox[2] / ratio;
                    let h = bbox[3] / ratio;
                    let x = cx - w / 2.;
                    let y = cy - h / 2.;
                    let y_bbox = Bbox::new(
                        Rect::from_xywh(
                            x.max(0.0f32).min(width_original),
                            y.max(0.0f32).min(height_original),
                            w,
                            h,
                        ),
                        id,
                        confidence,
                        self.names.as_ref().map(|names| names[id].to_owned()),
                    );

                    // kpts
                    let y_kpts = {
                        if let Some(kpts) = kpts {
                            let mut kpts_ = Vec::new();
                            for i in 0..self.nk {
                                let kx = kpts[KPT_STEP * i] / ratio;
                                let ky = kpts[KPT_STEP * i + 1] / ratio;
                                let kconf = kpts[KPT_STEP * i + 2];
                                if kconf < self.kconfs[i] {
                                    kpts_.push(Keypoint::default());
                                } else {
                                    kpts_.push(Keypoint::new(
                                        Point::new(
                                            kx.max(0.0f32).min(width_original),
                                            ky.max(0.0f32).min(height_original),
                                        ),
                                        kconf,
                                    ));
                                }
                            }
                            Some(kpts_)
                        } else {
                            None
                        }
                    };

                    // merged
                    data.push((y_bbox, y_kpts, coefs));
                }

                // nms
                if self.apply_nms {
                    non_max_suppression(&mut data, self.iou);
                }

                // decode
                let mut y_bboxes: Vec<Bbox> = Vec::new();
                let mut y_kpts: Vec<Vec<Keypoint>> = Vec::new();
                let mut y_masks: Vec<Vec<u8>> = Vec::new();
                for elem in data.into_iter() {
                    if let Some(kpts) = elem.1 {
                        y_kpts.push(kpts)
                    }

                    // decode masks
                    if let Some(coefs) = elem.2 {
                        let proto = protos.unwrap().slice(s![idx, .., .., ..]);
                        let (nm, nh, nw) = proto.dim();

                        // coefs * proto -> mask
                        let coefs = Array::from_shape_vec((1, nm), coefs)?; // (n, nm)
                        let proto = proto.to_owned().into_shape((nm, nh * nw))?; // (nm, nh*nw)
                        let mask = coefs.dot(&proto).into_shape((nh, nw, 1))?; // (nh, nw, n)

                        // build image from ndarray
                        let mask_im: ImageBuffer<image::Luma<_>, Vec<f32>> =
                            match ImageBuffer::from_raw(nw as u32, nh as u32, mask.into_raw_vec()) {
                                Some(image) => image,
                                None => panic!("can not create image from ndarray"),
                            };
                        let mut mask_im = image::DynamicImage::from(mask_im); // -> dyn

                        // rescale masks
                        let (_, w_mask, h_mask) =
                            ops::scale_wh(width_original, height_original, nw as f32, nh as f32);
                        let mask_cropped = mask_im.crop(0, 0, w_mask as u32, h_mask as u32);
                        let mask_original = mask_cropped.resize_exact(
                            width_original as u32,
                            height_original as u32,
                            image::imageops::FilterType::Triangle,
                        );

                        // crop-mask with bbox
                        let mut mask_original_cropped = mask_original.into_luma8();
                        for y in 0..height_original as usize {
                            for x in 0..width_original as usize {
                                if x < elem.0.xmin() as usize
                                    || x > elem.0.xmax() as usize
                                    || y < elem.0.ymin() as usize
                                    || y > elem.0.ymax() as usize
                                {
                                    mask_original_cropped.put_pixel(
                                        x as u32,
                                        y as u32,
                                        image::Luma([0u8]),
                                    );
                                }
                            }
                        }
                        y_masks.push(mask_original_cropped.into_raw());
                    }
                    y_bboxes.push(elem.0);
                }

                // save each result
                let y = Results {
                    probs: None,
                    bboxes: if !y_bboxes.is_empty() {
                        Some(y_bboxes)
                    } else {
                        None
                    },
                    keypoints: if !y_kpts.is_empty() {
                        Some(y_kpts)
                    } else {
                        None
                    },
                    masks: if !y_masks.is_empty() {
                        Some(y_masks)
                    } else {
                        None
                    },
                };
                ys.push(y);
            }

            Ok(ys)
        }
    }

    fn fetch_names(engine: &OrtEngine) -> Option<Vec<String>> {
        // fetch class names from onnx metadata
        // String format: `{0: 'person', 1: 'bicycle', 2: 'sports ball', ..., 27: "yellow_lady's_slipper"}`
        engine.try_fetch("names").map(|names| {
            let re = Regex::new(r#"(['"])([-()\w '"]+)(['"])"#).unwrap();
            let mut names_ = vec![];
            for (_, [_, name, _]) in re.captures_iter(&names).map(|x| x.extract()) {
                names_.push(name.to_string());
            }
            names_
        })
    }

    pub fn batch(&self) -> isize {
        self.batch.opt
    }

    pub fn width(&self) -> isize {
        self.width.opt
    }

    pub fn height(&self) -> isize {
        self.height.opt
    }
}