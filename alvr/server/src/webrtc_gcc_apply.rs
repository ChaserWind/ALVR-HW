use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant, self}, f64::{NAN, INFINITY}, 
};

use std::error::Error;
use std::fs::OpenOptions;
use std::io::prelude::*;
use csv::Writer;
use chrono::{Utc, TimeZone};




pub struct TimestampGroup{
    pub size: usize,
    pub first_timestamp: i64,
    pub timestamp: i64,
    pub first_arrival_ms: i64,
    pub complete_time_ms: i64,
    pub last_system_time_ms: i64,
}
impl Default for TimestampGroup {
    fn default() -> Self {
        Self {
            size: 0,
            first_timestamp: 0,
            timestamp: 0,
            first_arrival_ms: -1,
            complete_time_ms: -1,
            last_system_time_ms: -1,
        }
    }
    
}









pub struct InterArrival{
    pub kReorderedResetThreshold:i32,
    pub kArrivalTimeOffsetThresholdMs:i64,
    pub kTimestampGroupLengthTicks:i32,
    pub current_timestamp_group_:TimestampGroup,
    pub prev_timestamp_group_:TimestampGroup,
    pub timestamp_to_ms_coeff_:f64,
    pub num_consecutive_reordered_packets_:i64,

}
impl InterArrival{
    pub fn new(
        timestamp_group_length_ticks:i32,
        timestamp_to_ms_coeff:f64,
    ) -> Self {
        Self {
            kReorderedResetThreshold:3,
            kArrivalTimeOffsetThresholdMs:3000,
            kTimestampGroupLengthTicks:timestamp_group_length_ticks,
            current_timestamp_group_:TimestampGroup {
                    ..Default::default()
            },
            prev_timestamp_group_:TimestampGroup {
                ..Default::default()
        },
            timestamp_to_ms_coeff_:timestamp_to_ms_coeff,
            num_consecutive_reordered_packets_:0,

        }
    }

    pub fn ComputeDeltas(&mut self,timestamp: i64,arrival_time_ms: i64,system_time_ms:i64,packet_size: usize,timestamp_delta: &mut i64,
        arrival_time_delta_ms:&mut i64,packet_size_delta:&mut i64)->bool{
            let mut calculated_deltas=false;
            if self.current_timestamp_group_.complete_time_ms==-1{

                self.current_timestamp_group_.timestamp=timestamp;
                self.current_timestamp_group_.first_timestamp=timestamp;
                self.current_timestamp_group_.first_arrival_ms=arrival_time_ms;
            }else if !self.PacketInOrder(timestamp) {
                return false;
            } else if self.NewTimestampGroup(arrival_time_ms, timestamp) {
                // First packet of a later frame, the previous frame sample is ready.
                if self.prev_timestamp_group_.complete_time_ms >= 0 {
                  *timestamp_delta =
                      self.current_timestamp_group_.timestamp - self.prev_timestamp_group_.timestamp;
                  *arrival_time_delta_ms = self.current_timestamp_group_.complete_time_ms -
                                           self.prev_timestamp_group_.complete_time_ms;
                  // Check system time differences to see if we have an unproportional jump
                  // in arrival time. In that case reset the inter-arrival computations.
                  let mut  system_time_delta_ms =
                      self.current_timestamp_group_.last_system_time_ms -
                      self.prev_timestamp_group_.last_system_time_ms;
                  if *arrival_time_delta_ms - system_time_delta_ms >=
                      self.kArrivalTimeOffsetThresholdMs {
                        self.Reset();
                        return false;
                  }
                  if *arrival_time_delta_ms < 0 {
                    // The group of packets has been reordered since receiving its local
                    // arrival timestamp.
                    self.num_consecutive_reordered_packets_+=1;
                    if self.num_consecutive_reordered_packets_ >= self.kReorderedResetThreshold as i64 {
                      
                      self.Reset();
                    }
                    return false;
                  } else {
                    self.num_consecutive_reordered_packets_ = 0;
                  }
                  //RTC_DCHECK_GE(*arrival_time_delta_ms, 0);
                  *packet_size_delta = self.current_timestamp_group_.size as i64-self.prev_timestamp_group_.size as i64;
                    calculated_deltas = true;
                }
                self.prev_timestamp_group_.complete_time_ms = self.current_timestamp_group_.complete_time_ms;
                self.prev_timestamp_group_.first_arrival_ms = self.current_timestamp_group_.first_arrival_ms;
                self.prev_timestamp_group_.first_timestamp = self.current_timestamp_group_.first_timestamp;
                self.prev_timestamp_group_.last_system_time_ms= self.current_timestamp_group_.last_system_time_ms;
                self.prev_timestamp_group_.size= self.current_timestamp_group_.size;
                self.prev_timestamp_group_.timestamp = self.current_timestamp_group_.timestamp;
                // The new timestamp is now the current frame.
                self.current_timestamp_group_.first_timestamp = timestamp;
                self.current_timestamp_group_.timestamp = timestamp;
                self.current_timestamp_group_.first_arrival_ms = arrival_time_ms;
                self.current_timestamp_group_.size = 0;
              } else {
                if self.current_timestamp_group_.timestamp<timestamp{
                    self.current_timestamp_group_.timestamp=timestamp
                }
                
              }
              // Accumulate the frame size.
              self.current_timestamp_group_.size += packet_size;
              self.current_timestamp_group_.complete_time_ms = arrival_time_ms;
              self.current_timestamp_group_.last_system_time_ms = system_time_ms;
            
              return calculated_deltas;
            
        }
    
    pub fn PacketInOrder(&mut self,timestamp: i64)->bool{
        if self.current_timestamp_group_.complete_time_ms==-1 {
            return true;
          } else {
            
            let timestamp_diff =
                timestamp - self.current_timestamp_group_.first_timestamp;
            return timestamp_diff < 0x80000000;
          }
    }

    pub fn NewTimestampGroup(&mut self,arrival_time_ms:i64,timestamp: i64)->bool{
        if self.current_timestamp_group_.complete_time_ms==-1{
            return false;
        }else if self.BelongsToBurst(arrival_time_ms, timestamp){
            return false;
        }
        else{
            let timestamp_diff =timestamp - self.current_timestamp_group_.first_timestamp;
            return timestamp_diff>self.kTimestampGroupLengthTicks as i64;
        }
    }


    pub fn Reset(&mut self)
    {
        self.num_consecutive_reordered_packets_=0;
        self.current_timestamp_group_=TimestampGroup {
            ..Default::default()
        };
        self.prev_timestamp_group_=TimestampGroup {
            ..Default::default()
        };
    }

    pub fn BelongsToBurst( &mut self,arrival_time_ms:i64,
        timestamp:i64) ->bool {
        //RTC_DCHECK_GE(current_timestamp_group_.complete_time_ms, 0);
        // let mut arrival_time_delta_ms =
        // arrival_time_ms - self.current_timestamp_group_.complete_time_ms;
        // let mut timestamp_diff = timestamp - self.current_timestamp_group_.timestamp;
        // let mut ts_delta_ms = (self.timestamp_to_ms_coeff_ * timestamp_diff as f64 + 0.5) as i64;
        // if ts_delta_ms == 0{
        //     return true;
        // }
        
        // let mut propagation_delta_ms = arrival_time_delta_ms - ts_delta_ms;
        // if propagation_delta_ms < 0 &&
        // arrival_time_delta_ms <= 5 &&
        // arrival_time_ms - self.current_timestamp_group_.first_arrival_ms <
        // 100{
        //     return true;
        // }
        
        return false;
        
        
    }
    

}

#[derive(PartialEq)]
pub enum BandwidthUsage {
    kBwNormal = 0,
    kBwUnderusing = 1,
    kBwOverusing = 2,
    kLast,
}


pub struct PacketTiming{
    pub arrival_time_ms:f64,
    pub smoothed_delay_ms:f64,
    pub raw_delay_ms:f64,
}
impl PacketTiming {
    fn new(arrival_time_ms: f64, smoothed_delay_ms: f64, raw_delay_ms: f64) -> Self {
        Self {
            arrival_time_ms,
            smoothed_delay_ms,
            raw_delay_ms,
        }
    }
}

pub fn LinearFitSlope(packets:& VecDeque<PacketTiming>)->Option<f64> {
  if packets.len()>2 {
        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        for packet in packets {
            sum_x += packet.arrival_time_ms;
            sum_y += packet.smoothed_delay_ms;
        }
        let mut x_avg = sum_x / packets.len() as f64;
        let mut y_avg = sum_y / packets.len() as f64;
        // 斜率计算 k = \sum (x_i-x_avg)(y_i-y_avg) / \sum (x_i-x_avg)^2
        let mut numerator = 0.0;
        let mut denominator = 0.0;
        for packet in packets {
            let mut  x = packet.arrival_time_ms;
            let mut y = packet.smoothed_delay_ms;
            numerator += (x - x_avg) * (y - y_avg);
            denominator += (x - x_avg) * (x - x_avg);
        }
        if denominator == 0.0{
            return Option::None;
        }
            
        return Some(numerator / denominator);
  }else{
    return Option::None;
  }
}










pub struct TrendlineEstimator{
    //TrendlineEstimatorSettings settings_;
    pub smoothing_coef_: f64,
    pub threshold_gain_: f64,
    // Used by the existing threshold.
    pub num_of_deltas_: i64,
    // Keep the arrival times small by using the change from the first packet.
    pub first_arrival_time_ms_: i64,
    // Exponential backoff filtering.
    pub accumulated_delay_: f64,
    pub smoothed_delay_: f64,
    // Linear least squares regression.
    

    pub k_up_:f64,
    pub k_down_:f64,
    pub overusing_time_threshold_: f64,
    pub threshold_: f64,
    pub prev_modified_trend_: f64,
    pub last_update_ms_: i64,
    pub prev_trend_:f64,
    pub time_over_using_:f64,
    pub overuse_counter_: i64,
    pub  hypothesis_: BandwidthUsage,
    pub  hypothesis_predicted_: BandwidthUsage,
    //NetworkStatePredictor* network_state_predictor_;
    pub  delay_hist_:VecDeque<PacketTiming>,
    pub current_trend_for_testing:f64,
    pub current_threshold_for_testing:f64,
}
impl TrendlineEstimator{
    pub fn new() -> Self {
        Self {
            
            smoothing_coef_: 0.9,
            threshold_gain_: 4.0,
            // Used by the existing threshold.
            num_of_deltas_: 0,
            // Keep the arrival times small by using the change from the first packet.
            first_arrival_time_ms_: -1,
            // Exponential backoff filtering.
            accumulated_delay_: 0.0,
            smoothed_delay_: 0.0,
            // Linear least squares regression.
            

            k_up_:0.0087,
            k_down_:0.039,
            overusing_time_threshold_: 10.0,
            threshold_: 12.5,//12.5
            prev_modified_trend_: NAN,
            last_update_ms_: -1,
            prev_trend_:0.0,
            time_over_using_:-1.0,
            overuse_counter_: 0,
            hypothesis_: BandwidthUsage::kBwNormal,
            hypothesis_predicted_: BandwidthUsage::kBwNormal,
            //NetworkStatePredictor* network_state_predictor_;
            delay_hist_:VecDeque::new(),
            current_threshold_for_testing:0.0,
            current_trend_for_testing:0.0,

        }
    }
    

    pub fn UpdateThreshold(&mut self,modified_trend:f64,
        now_ms: i64) {
        if self.last_update_ms_ == -1{
            self.last_update_ms_ = now_ms;
        }
        

        if modified_trend.abs() > self.threshold_ + 15.0 {
        // 如果出现了异常的延迟梯度升高，不予更新
        self.last_update_ms_ = now_ms;
        return;
        }

        // 延迟梯度降低时快速降低阈值，延迟梯度升高时缓慢增加阈值（保证竞争性）
        // k_down_= 0.039
        // k_up_ = 0.0087
        let k = if modified_trend.abs() < self.threshold_ {
            self.k_down_
        } else {
            self.k_up_
        };
        
        let  kMaxTimeDeltaMs = 100;
        let mut time_delta_ms = std::cmp::min(now_ms - self.last_update_ms_, kMaxTimeDeltaMs);
        self.threshold_ += k * (modified_trend.abs() - self.threshold_) * time_delta_ms as f64;
        
        if self.threshold_>600.0 as f64{
            self.threshold_=600.0;
        }else if self.threshold_<6.0 as f64{
            self.threshold_=6.0;
        }
        self.last_update_ms_ = now_ms;
    }


    pub fn Detect( &mut self,trend: f64,ts_delta: f64, now_ms: i64) {
        // 如果包组数量不够（刚启动），认为当前是正常状态
        if self.num_of_deltas_ < 2 {
          self.hypothesis_ = BandwidthUsage::kBwNormal;
          return;
        }

        // 乘以包组数量以及阈值增益，因为计算得到的trend常常是一个非常小的值
        let modified_trend =
            std::cmp::min(self.num_of_deltas_, 60) as f64 * trend * self.threshold_gain_;
        self.prev_modified_trend_ = modified_trend;
        
        // modified_trend > threshold_，且持续一段时间，同时这段时间内，modified_trend没有变小趋势，认为处于overuse状态
        // modified_trend < -threshold_，认为处于underuse状态
        // -threshold_ <= modified_trend <= threshold_，认为处于normal状态
        if modified_trend > self.threshold_ {
          if self.time_over_using_ == -1.0 {
            // 初始化计时器，认为从上次发送包开始已经有一半的时间处于over_using的状态
            self.time_over_using_ = ts_delta / 2.0;
          } else {
            // 计时器已经被初始化，则直接增加当前发送的时间间隔，这段时间都是over_using
            self.time_over_using_ += ts_delta;
          }
          self.overuse_counter_+=1;

          // 如果：
          //（1）总的over_using的时间超过阈值，
          //（2）并且over_use的次数超过两次，
          //（3）并且当前计算出来的trend要比之前计算出来的trend更大。
          // 认为当前网络处于overusing状态
          if (self.time_over_using_ > self.overusing_time_threshold_ && self.overuse_counter_ > 1) {
            if trend >= self.prev_trend_ {
              self.time_over_using_ = 0.0;
              self.overuse_counter_ = 0;
              self.hypothesis_ = BandwidthUsage::kBwOverusing;
            }
          }
        // modified_trend < -threshold_，认为处于underuse状态
        } else if modified_trend < -self.threshold_ {
          self.time_over_using_ = -1.0;
          self.overuse_counter_ = 0;
          self.hypothesis_ = BandwidthUsage::kBwUnderusing;
        // -threshold_ <= modified_trend <= threshold_，认为处于normal状态
        } else {
          self.time_over_using_ = -1.0;
          self.overuse_counter_ = 0;
          self.hypothesis_ = BandwidthUsage::kBwNormal;
        }
        self.current_threshold_for_testing=self.threshold_;
        self.current_trend_for_testing=modified_trend;
        self.prev_trend_ = trend;
        // 为了和网络中存在的竞争流竞争带宽，更新动态阈值
        self.UpdateThreshold(modified_trend, now_ms);
      }
      




    pub fn UpdateTrendline(&mut self,recv_delta_ms:f64,
        send_delta_ms:f64,
        send_time_ms:i64,
        arrival_time_ms:i64,
        packet_size:i64) {
            // 根据当前最新一帧和前一帧的发送和接收时间差计算时延梯度
                let delta_ms = recv_delta_ms - send_delta_ms;
                self.num_of_deltas_+=1;
                self.num_of_deltas_ = std::cmp::min(self.num_of_deltas_, 1000);
                if self.first_arrival_time_ms_ == -1{
                    self.first_arrival_time_ms_ = arrival_time_ms;
                }
                

                // 计算累计的时延梯度并且用EWMA做平滑
                self.accumulated_delay_ += delta_ms;
                self.smoothed_delay_ = self.smoothing_coef_ * self.smoothed_delay_ +
                (1.0 - self.smoothing_coef_) * self.accumulated_delay_;
                
                // delay_hist_中保存了历史的（时间，时延梯度累积量）的数据
                self.delay_hist_.push_back(PacketTiming::new((arrival_time_ms-self.first_arrival_time_ms_) as f64,
                self.smoothed_delay_,self.accumulated_delay_));
                if self.delay_hist_.len()> 20{
                    self.delay_hist_.pop_front();
                }
                

                // 如果历史数据量大于20，使用一个线性拟合计算斜率为当前的拥塞情况（trend）
                let mut  trend = self.prev_trend_;
                if self.delay_hist_.len() == 20 {
                    // 使用线性拟合拟合斜率trend，trend的实际物理含义可以近似理解成(send_rate - capacity)/capacity.
                    // 0 < trend < 1 -------> 当前delay正在升高，队列正在被填充
                    // trend == 0    -------> 当前delay没有发生变化
                    // trend < 0     -------> 延迟正在减少，队列正在被清空
                trend = LinearFitSlope(&self.delay_hist_).unwrap_or(trend);
                }
                // 根据当前时延变化的增长趋势判断网络的拥塞状况
                self.Detect(trend, send_delta_ms, arrival_time_ms);
        }
                

                
}
          
pub struct LinkCapacityEstimator{
    pub estimate_kbps_:Option<f64>,
    pub deviation_kbps_:f64,
}
impl LinkCapacityEstimator{
    pub fn new()->Self{
        Self { estimate_kbps_: Option::None, deviation_kbps_: 0.4}
    }
    pub fn deviation_estimate_kbps(&mut self)->f64 {
        return (self.deviation_kbps_*self.estimate_kbps_.unwrap()).sqrt();
    }
    pub fn  UpperBound(&mut self)->f64 {
        if !self.estimate_kbps_.is_none()
        {
            return (self.estimate_kbps_.unwrap()+ 3.0 * self.deviation_estimate_kbps())*1024.0;
        }
          
        return f64::INFINITY;
      }
      
      pub fn LowerBound(&mut self)->f64 {
        if !self.estimate_kbps_.is_none(){
            return f64::max(0.0, self.estimate_kbps_.unwrap()-3.0*self.deviation_estimate_kbps())*1024.0;
        }
          
        return 0.0;
      }
      
      pub fn Reset(&mut self){
        self.estimate_kbps_=Option::None;
      }
      
      pub fn OnOveruseDetected(&mut self, acknowledged_rate:f64) {
        self.Update(acknowledged_rate, 0.05);
      }
      
      pub fn OnProbeRate(&mut self, probe_rate:f64) {
        self.Update(probe_rate, 0.5);
      }
      
      pub fn Update(&mut self,capacity_sample:f64, alpha:f64) {
        let mut sample_kbps = capacity_sample*0.001;
        if self.estimate_kbps_.is_none() {
          self.estimate_kbps_ = Some(sample_kbps);
        } else {
          self.estimate_kbps_ = Some((1.0 - alpha) * self.estimate_kbps_.unwrap() + alpha * sample_kbps);
        }
        // Estimate the variance of the link capacity estimate and normalize the
        // variance with the link capacity estimate.
        let norm = f64::max(self.estimate_kbps_.unwrap(), 1.0);
        let mut error_kbps = self.estimate_kbps_.unwrap() - sample_kbps;
        self.deviation_kbps_ =
            (1.0 - alpha) * self.deviation_kbps_ + alpha * error_kbps * error_kbps / norm;
        // 0.4 ~= 14 kbit/s at 500 kbit/s
        // 2.5f ~= 35 kbit/s at 500 kbit/s
        if self.deviation_kbps_>2.5 as f64{
            self.deviation_kbps_=2.5;
        }else if self.deviation_kbps_<0.4 as f64{
            self.deviation_kbps_=0.4;//wanggai 1.0,shishi 1.5
        }
        
      }
      
      pub fn has_estimate(&mut self) ->bool {
        return !self.estimate_kbps_.is_none();
      }
      
      pub fn  estimate(&mut self) ->f64 {
        return self.estimate_kbps_.unwrap()*1024.0;
      }
      
      
}
pub struct NetworkStateEstimate{
    pub confidence:f64,
    pub update_time:i64,
    pub last_receive_time:i64,
    pub last_send_time:i64,
    pub link_capacity:f64,
    pub link_capacity_lower:f64,
    pub link_capacity_upper:f64,
    pub pre_link_buffer_delay:i64,
    pub post_link_buffer_delay:i64,
    pub propagation_delay:i64,
}
impl Default for NetworkStateEstimate {
    fn default() -> Self {
        Self {
            confidence:f64::NAN,
            update_time:i64::MIN ,
            last_receive_time:i64::MIN,
            last_send_time:i64::MIN,
            link_capacity:-std::f64::INFINITY,
            link_capacity_lower:-std::f64::INFINITY,
            link_capacity_upper:-std::f64::INFINITY,
            pre_link_buffer_delay:i64::MIN,
            post_link_buffer_delay:i64::MIN,
            propagation_delay:i64::MIN,
        }
    }
}

#[derive(PartialEq)]
pub enum RateControlState {
    kRcHold = 0,
    kRcIncrease = 1,
    kRcDecrease = 2,
    
}
pub struct RateControlInput{
    pub bw_state:BandwidthUsage,
    pub estimated_throughput:Option<f64>,
}
impl RateControlInput{
    pub fn new(bw_state:BandwidthUsage,
        estimated_throughput:Option<f64>)->Self{
            Self{
                bw_state:bw_state,
                estimated_throughput:estimated_throughput,
            }
            
        }
}

pub struct AimdRateControl{
    pub min_configured_bitrate_:f64,
    pub max_configured_bitrate_:f64,
    pub current_bitrate_:f64,
    pub latest_estimated_throughput_:f64,
    pub link_capacity_:LinkCapacityEstimator,
    pub network_estimate_:Option<NetworkStateEstimate>,
    pub rate_control_state_:RateControlState,
    pub time_last_bitrate_change_:i64,
    pub time_last_bitrate_decrease_:i64,
    pub time_first_throughput_estimate_:i64,
    pub bitrate_is_initialized_:bool,
    pub beta_:f64,
    pub in_alr_:bool,
    pub rtt_:i64,
    pub send_side_:bool,
    pub no_bitrate_increase_in_alr_:bool,
    pub last_decrease_:Option<f64>,
    pub esitmate_thr_testing:f64,
    pub flag_for_qp:u64,
    pub normalize_delta:f64

}
impl AimdRateControl{
    pub fn new(send_side:bool)->Self{
        Self{
            min_configured_bitrate_:50000.0,
            max_configured_bitrate_:150.0*1024.0*1024.0,
            current_bitrate_:30000.0*1024.0,
            latest_estimated_throughput_:30000.0*1024.0,
            link_capacity_:LinkCapacityEstimator::new() ,
            rate_control_state_:RateControlState::kRcHold,
            time_last_bitrate_change_:i64::MIN,
            time_last_bitrate_decrease_:i64::MIN,
            time_first_throughput_estimate_:i64::MIN,
            bitrate_is_initialized_:false,
            beta_:0.85,
            in_alr_:false,
            rtt_:200,
            send_side_:send_side,
            no_bitrate_increase_in_alr_:true,
            last_decrease_:Some(0.0),
            network_estimate_:Some(NetworkStateEstimate {
                ..Default::default()
            }),
            esitmate_thr_testing:0.0,
            flag_for_qp:1,
            normalize_delta:0.0,
        }

    }
    pub fn ChangeState(&mut self,input:& RateControlInput,
        at_time:i64) {
            match input.bw_state {
                BandwidthUsage::kBwNormal => {
                    if self.rate_control_state_ == RateControlState::kRcHold {
                        self.time_last_bitrate_change_ = at_time;
                        self.rate_control_state_ = RateControlState::kRcIncrease;
                    }
                },
                BandwidthUsage::kBwOverusing => {
                    if self.rate_control_state_ != RateControlState::kRcDecrease {
                        self.rate_control_state_ = RateControlState::kRcDecrease;
                    }
                },
                BandwidthUsage::kBwUnderusing => {
                    self.rate_control_state_ = RateControlState::kRcHold;
                },
                _ => {
                    
                }
            }
        
    }
    pub fn GetNearMaxIncreaseRateBpsPerSecond(&mut self) -> f64 {
        //RTC_DCHECK(!current_bitrate_.IsZero());
        let kFrameInterval = 0.015;
        let mut frame_size = self.current_bitrate_ * kFrameInterval;
        let kPacketSize = 1200*8;//wz tiaoshi
        let packets_per_frame = frame_size as f64/ kPacketSize as f64;
        let  avg_packet_size = frame_size / packets_per_frame;
      
        // Approximate the over-use estimator delay to 100 ms.
        let mut response_time = (self.rtt_ + 100) as f64*0.001;
      
        //response_time = response_time * 2;
        let mut increase_rate_bps_per_second =
            avg_packet_size / response_time as f64;
        let kMinIncreaseRateBpsPerSecond = 4000.0;
        return f64::max(kMinIncreaseRateBpsPerSecond, increase_rate_bps_per_second);
      }

    pub fn MultiplicativeRateIncrease(&mut self,
        at_time:i64,
         last_time:i64,
         current_bitrate:f64) ->f64 {
      let mut alpha = 1.08 as f64;
      if last_time==i64::MIN {
        let mut time_since_last_update = at_time - last_time;
        alpha = alpha.powf(((time_since_last_update as f64/1000.0).min(1.0))as f64);
      }
      let mut multiplicative_increase =
          f64::max(current_bitrate * (alpha - 1.0), 1000.0);
      return multiplicative_increase;
    }
    
    pub fn AdditiveRateIncrease(&mut self,at_time:i64,
                                                    last_time:i64) ->f64 {
      let mut time_period_seconds = ((at_time - last_time)as f64)/1000.0;
      let mut data_rate_increase_bps =
          self.GetNearMaxIncreaseRateBpsPerSecond() * time_period_seconds;
      return data_rate_increase_bps;
    }
    
    pub fn Update(&mut self,input:&RateControlInput,
         at_time:i64)-> f64 {
        if !self.bitrate_is_initialized_ {
        let kInitializationTime = 5000 as i64;
        //RTC_DCHECK_LE(kBitrateWindowMs, kInitializationTime.ms());
        if self.time_first_throughput_estimate_==i64::MIN {
        if input.estimated_throughput.is_some(){
            self.time_first_throughput_estimate_ = at_time;
        }
        
        } else if at_time - self.time_first_throughput_estimate_ >
        kInitializationTime &&
        input.estimated_throughput.is_some() {
        self.current_bitrate_ = input.estimated_throughput.unwrap();
        self.bitrate_is_initialized_ = true;
        }
        }

        self.ChangeBitrate(input, at_time);
        return self.current_bitrate_;
    }

    pub fn ClampBitrate(&mut self,new_bitrate: f64) ->f64 {
        let mut new_bitrate_r=new_bitrate;
        if self.network_estimate_.is_some()&&
            self.network_estimate_.as_mut().unwrap().link_capacity_upper!=-std::f64::INFINITY {
          let mut upper_bound = self.network_estimate_.as_mut().unwrap().link_capacity_upper;
          new_bitrate_r = f64::min(upper_bound, new_bitrate_r);
        }
        if self.network_estimate_.is_some()&& self.network_estimate_.as_mut().unwrap().link_capacity_lower!=-std::f64::INFINITY &&
            new_bitrate_r < self.current_bitrate_{
                new_bitrate_r = f64::min(
              self.current_bitrate_,
              f64::max(new_bitrate_r, self.network_estimate_.as_mut().unwrap().link_capacity_lower * self.beta_));
        }
        new_bitrate_r = f64::max(new_bitrate_r, self.min_configured_bitrate_);
        return new_bitrate_r;
      }

    pub fn ChangeBitrate(&mut self,input:& RateControlInput,
         at_time:i64) {
            let mut new_bitrate=Option::None;
            let mut estimated_throughput =
            input.estimated_throughput.unwrap_or(self.latest_estimated_throughput_);
            if input.estimated_throughput.is_some(){
                self.latest_estimated_throughput_ = input.estimated_throughput.unwrap();
            }
            
            if !self.bitrate_is_initialized_ &&
            input.bw_state != BandwidthUsage::kBwOverusing{
                return;
            }
            
            self.ChangeState(input, at_time);

            match self.rate_control_state_{
                RateControlState::kRcHold=>{},
                RateControlState::kRcIncrease => { 
                    // ACK码率超过链路上限，超出99%置信区间，当前的链路瓶颈可能变化了，需要重新检测
                    if estimated_throughput > self.link_capacity_.UpperBound()
                    { 
                        self.link_capacity_.Reset(); 
                    }

                    // 限制最大估计码率为ACK码率的1.5倍，避免没有限制的增加
                    // 额外增加10kbps是为了避免在低带宽时增加太慢
                    let mut increase_limit = 1.5 * estimated_throughput + 10240.0;
                    increase_limit = std::f64::INFINITY;
                    if self.send_side_ && self.in_alr_ && self.no_bitrate_increase_in_alr_ {
                        increase_limit = self.current_bitrate_;
                        increase_limit = std::f64::INFINITY;
                    }

                    if self.current_bitrate_ < increase_limit {
                        let mut increased_bitrate = -std::f64::INFINITY;
                         // 之前已经估计出链路的瓶颈了（link capacity有结果），此时的增加需要谨慎，因此需要线性增加
                        if self.link_capacity_.has_estimate() {
                            let additive_increase = self.AdditiveRateIncrease(at_time, self.time_last_bitrate_change_);
                            increased_bitrate = self.current_bitrate_ + additive_increase;
                        } else {
                            // 压根没有达到链路瓶颈，没有出现过overuse，可以加快速度估计，这是可以1.08倍指数级增加
                            let multiplicative_increase = self.MultiplicativeRateIncrease(
                                at_time, self.time_last_bitrate_change_, self.current_bitrate_
                            );
                            increased_bitrate = self.current_bitrate_ + multiplicative_increase;
                        }
                        new_bitrate = Some(increased_bitrate.min(increase_limit));
                    }
                    self.time_last_bitrate_change_ = at_time;
                    },
                
                
                RateControlState::kRcDecrease => {
                    let mut decreased_bitrate = std::f64::INFINITY;
                    // beta默认0.85，降低按照0.85倍降低，按照估计的接收速率的0.85倍
                    decreased_bitrate = estimated_throughput * self.beta_;
                    // 如果比当前的估计带宽还要大，那么按照链路能力的0.85倍，链路能力估计较为稳定，也较低
                    if decreased_bitrate > self.current_bitrate_  {
                        if self.link_capacity_.has_estimate() {
                            decreased_bitrate = self.beta_ * self.link_capacity_.estimate();
                        }
                    }

                    if decreased_bitrate < self.current_bitrate_ {
                        new_bitrate = Some(decreased_bitrate);
                    }

                    if self.bitrate_is_initialized_ && estimated_throughput < self.current_bitrate_ {
                        if let Some(new_br) = new_bitrate {
                            self.last_decrease_ = Some(self.current_bitrate_ - new_br);
                        } else {
                            self.last_decrease_ = Option::None;
                        }
                    }
                    
                    // 当前的估计码率低于link capacity下限，即超过99%置信区间，我们对link capacity的准确度标识怀疑
                    if estimated_throughput < self.link_capacity_.LowerBound() {
                        self.link_capacity_.Reset();
                    }

                    self.bitrate_is_initialized_ = true;
                    self.esitmate_thr_testing=estimated_throughput;
                    // 每次overuse降低带宽的时候更新link capacity
                    self.link_capacity_.OnOveruseDetected(estimated_throughput);
                    // 降低后先进入hold状态
                    self.rate_control_state_ = RateControlState::kRcHold;
                    self.time_last_bitrate_change_ = at_time;
                    self.time_last_bitrate_decrease_ = at_time;
                },
                _ => {

                },
                
            }
            if self.network_estimate_.is_none(){
                self.network_estimate_=Some(NetworkStateEstimate {
                    ..Default::default()
                });
            }
            if self.link_capacity_.LowerBound()!=0.0{
                if self.link_capacity_.LowerBound()<self.min_configured_bitrate_{
                    self.network_estimate_.as_mut().unwrap().link_capacity_lower=self.min_configured_bitrate_;
                }
                else{
                    self.network_estimate_.as_mut().unwrap().link_capacity_lower=self.link_capacity_.LowerBound();
                }
            }else {
                self.network_estimate_.as_mut().unwrap().link_capacity_lower=self.min_configured_bitrate_;
            }
            if self.link_capacity_.UpperBound()!=f64::INFINITY{
                if self.link_capacity_.UpperBound()>self.max_configured_bitrate_{
                    self.network_estimate_.as_mut().unwrap().link_capacity_upper=self.max_configured_bitrate_;
                }
                else{
                    self.network_estimate_.as_mut().unwrap().link_capacity_upper=self.link_capacity_.UpperBound();
                }
            }else {
                self.network_estimate_.as_mut().unwrap().link_capacity_upper=self.max_configured_bitrate_;
            }

            self.current_bitrate_ = self.ClampBitrate(new_bitrate.unwrap_or(self.current_bitrate_));
        }


        
    

}



pub struct BitrateEstimator{
    pub sum_ :i64,
    pub initial_window_ms_:i64,
    pub noninitial_window_ms_:i64,
    pub uncertainty_scale_:f64,
    pub uncertainty_scale_in_alr_:f64,
    pub small_sample_uncertainty_scale_:f64,
    pub small_sample_threshold_:usize,
    pub uncertainty_symmetry_cap_:usize,
    pub estimate_floor_:usize,
    pub current_window_ms_:i64,
    pub prev_time_ms_:i64,
    pub bitrate_estimate_kbps_:f64,
    pub bitrate_estimate_var_:f64,
}
impl BitrateEstimator{
    pub fn new()->Self{
        Self{
            sum_:0,
            initial_window_ms_:350,
            noninitial_window_ms_:250,
            uncertainty_scale_:10.0,
            uncertainty_scale_in_alr_:10.0,
            small_sample_uncertainty_scale_:10.0,
            small_sample_threshold_:0,
            uncertainty_symmetry_cap_:0,
            estimate_floor_:0,
            current_window_ms_:0,
            prev_time_ms_:-1,
            bitrate_estimate_kbps_:-1.0,
            bitrate_estimate_var_:50.0,
        }
    }
    pub fn UpdateWindow( &mut self,now_ms:i64,
         bytes:usize,
         rate_window_ms:i64,
         is_small_sample:&mut bool)->f64{
            if now_ms < self.prev_time_ms_ {
                self.prev_time_ms_ = -1;
                self.sum_ = 0;
                self.current_window_ms_ = 0;
              }
              if self.prev_time_ms_ >= 0 {
                //更新当前时间窗口的长度
                self.current_window_ms_ += now_ms - self.prev_time_ms_;

                // 如果超过一个时间窗口内没有收到任何数据包组，这个时间窗口就不能被用来计算接收速率
                if now_ms - self.prev_time_ms_ > rate_window_ms {
                    self.sum_ = 0;
                    self.current_window_ms_ %= rate_window_ms;
                }
              }
              self.prev_time_ms_ = now_ms;
              let mut bitrate_sample = -1.0;
              // 当前时间窗口超过了统计间隔，则用来计算即时的接收速率
              if self.current_window_ms_ >= rate_window_ms {
                // 如果这个时间窗口内收到的数据量小于阈值，认为是一个接收速率小的样本
                *is_small_sample = self.sum_ < self.small_sample_threshold_ as i64;
                bitrate_sample = 8.0 * self.sum_ as f64 / rate_window_ms as f64;
                self.current_window_ms_ -= rate_window_ms;
                self.sum_ = 0;
              }
              self.sum_ += bytes as i64;
              // 返回按照滑动窗口统计的接收速率
              return bitrate_sample;
         }

    pub fn Update(&mut self, at_time:i64, first_packet_receive_time:i64,  amount:usize,  in_alr:bool){
        let mut rate_window_ms = self.noninitial_window_ms_;
        // 在一开始没有接收带宽估计的时候，使用一个更大的统计时间窗口（350ms）来保证得到一个稳定的估计值,
        // 如果已经有了带宽的估计，则使用250ms作为统计时间窗口
        if self.bitrate_estimate_kbps_ < 0.0{
            rate_window_ms = self.initial_window_ms_;
        }
            
        let mut is_small_sample = false;
        //let mut bitrate_sample_kbps = self.UpdateWindow(at_time, amount,
        //                                        rate_window_ms, &mut is_small_sample);

        let mut bitrate_sample_kbps = -1.0;

        //8.6:尝试采用burst的方式采样接收速率
        if at_time > first_packet_receive_time{
            bitrate_sample_kbps = 8.0 * amount as f64 / (at_time as f64 - first_packet_receive_time as f64);
        }

        if bitrate_sample_kbps < 0.0
        {
            return;
        }
            
        if self.bitrate_estimate_kbps_ < 0.0{
            // 初始化：如果还没有接收带宽的估计，就直接用采到的样本。
            self.bitrate_estimate_kbps_ = bitrate_sample_kbps;
            return;
        }
        // 对于：
        // (1)采集到的速率异常低，并且小于当前估计值的sample，
        // (2)处于applimit阶段，并且小于当前估计值的sample，
        // 增加不确定性权重
        let mut scale = self.uncertainty_scale_;
        if is_small_sample && bitrate_sample_kbps < self.bitrate_estimate_kbps_ {
            scale = self.small_sample_uncertainty_scale_;
        } else if in_alr && bitrate_sample_kbps < self.bitrate_estimate_kbps_ {
            scale = self.uncertainty_scale_in_alr_;
        }
        // 根据sample的速率和当前估计值的距离计算当前样本的不确定性
        let mut sample_uncertainty =
            scale * (self.bitrate_estimate_kbps_ - bitrate_sample_kbps).abs() /
            (self.bitrate_estimate_kbps_ +
            f64::min(bitrate_sample_kbps,
                        self.uncertainty_symmetry_cap_ as f64));

        let mut  sample_var = sample_uncertainty * sample_uncertainty;
        // 使用一个贝叶斯估计接收带宽的值，如果样本的不确定性偏高，就赋予更低的权重
        let mut pred_bitrate_estimate_var = self.bitrate_estimate_var_ + 5.0;
        self.bitrate_estimate_kbps_ = (sample_var * self.bitrate_estimate_kbps_ +
                                    pred_bitrate_estimate_var * bitrate_sample_kbps) /
                                (sample_var + pred_bitrate_estimate_var);
        self.bitrate_estimate_kbps_ =
            f64::max(self.bitrate_estimate_kbps_, self.estimate_floor_ as f64);
        self.bitrate_estimate_var_ = sample_var * pred_bitrate_estimate_var /
                                (sample_var + pred_bitrate_estimate_var);
        //BWE_TEST_LOGGING_PLOT(1, "acknowledged_bitrate", at_time.ms(),
                                //bitrate_estimate_kbps_ * 1000);
    }
    pub fn bitrate(&mut self)->Option<f64>{
        if self.bitrate_estimate_kbps_<0.0{
            return Option::None;
        }
        else {
            return Some(self.bitrate_estimate_kbps_*1024.0);
        }
    }
    pub fn PeekRate(&mut self)->Option<f64>{
        if self.current_window_ms_>0{
            return Some(self.sum_ as f64/(self.current_window_ms_ as f64*0.001));
        }
        else {
            return Option::None;
        }
    }
    pub fn ExpectFastRateChange(&mut self){
        self.bitrate_estimate_var_+=200.0;
    }
}