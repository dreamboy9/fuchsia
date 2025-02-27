// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/queue.h"

#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>

#include "src/developer/forensics/crash_reports/constants.h"
#include "src/developer/forensics/crash_reports/info/queue_info.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace forensics {
namespace crash_reports {

Queue::Queue(async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
             std::shared_ptr<InfoContext> info_context, LogTags* tags, CrashServer* crash_server,
             SnapshotManager* snapshot_manager)
    : dispatcher_(dispatcher),
      services_(services),
      tags_(tags),
      store_(tags_, info_context, /*temp_root=*/Store::Root{kStoreTmpPath, kStoreMaxTmpSize},
             /*persistent_root=*/Store::Root{kStoreCachePath, kStoreMaxCacheSize}),
      crash_server_(crash_server),
      snapshot_manager_(snapshot_manager),
      info_(std::move(info_context)) {
  FX_CHECK(dispatcher_);
  FX_CHECK(crash_server_);

  upload_all_every_fifteen_minutes_task_.set_handler([this]() { UploadAllEveryFifteenMinutes(); });

  // Note: The upload attempt data is lost when the component stops and all reports start with
  // upload attempts of 0.
  for (const auto& report_id : store_.GetReports()) {
    // It could technically be an hourly snapshot, but the snapshot has not been persisted so it is
    // okay to have another one here.
    pending_reports_.emplace_back(report_id, false /*not a known hourly report*/);
  }
  std::sort(pending_reports_.begin(), pending_reports_.end(),
            [](const PendingReport& lhs, const PendingReport& rhs) {
              return lhs.report_id <= rhs.report_id;
            });

  if (!pending_reports_.empty()) {
    std::vector<std::string> report_id_strs;
    for (const auto& pending_report : pending_reports_) {
      report_id_strs.push_back(std::to_string(pending_report.report_id));
    }
    FX_LOGS(INFO) << "Initializing queue with reports: " << fxl::JoinStrings(report_id_strs, " ");
  }
}

bool Queue::Contains(const ReportId report_id) const {
  return std::find_if(pending_reports_.cbegin(), pending_reports_.cend(),
                      [=](const PendingReport& r) { return r.report_id == report_id; }) !=
         pending_reports_.cend();
}

bool Queue::HasHourlyReport() const {
  return std::find_if(pending_reports_.cbegin(), pending_reports_.cend(),
                      [=](const PendingReport& r) { return r.is_hourly_report; }) !=
         pending_reports_.cend();
}

void Queue::StopUploading() {
  stop_uploading_ = true;
  upload_all_every_fifteen_minutes_task_.Cancel();
}

bool Queue::Add(Report report) {
  if (reporting_policy_ == ReportingPolicy::kDoNotFileAndDelete) {
    info_.MarkReportAsDeleted(0u);
    return true;
  }

  // Attempt to upload a report before putting it in the store.
  if (reporting_policy_ == ReportingPolicy::kUpload && !stop_uploading_) {
    if (Upload(report)) {
      return true;
    }
  }

  PendingReport pending_report(report.Id(), report.IsHourlyReport());

  // If an hourly report is already present, don't delete it and don't store a new one. This is
  // done to preserve the data from the report_id hourly report that wasn't successfully uploaded
  // and will have the best chance of containing data on why.
  if (report.IsHourlyReport() && HasHourlyReport()) {
    Delete(report.Id());
    return true;
  }

  std::vector<ReportId> garbage_collected_reports;
  const bool success = store_.Add(std::move(report), &garbage_collected_reports);

  for (const auto& id : garbage_collected_reports) {
    GarbageCollect(id);
    if (auto remove = std::remove_if(pending_reports_.begin(), pending_reports_.end(),
                                     [=](const PendingReport& r) { return r.report_id == id; });
        remove != pending_reports_.end()) {
      pending_reports_.erase(remove, pending_reports_.end());
    }
  }

  if (!success) {
    FreeResources(pending_report.report_id);
    return false;
  }

  if (reporting_policy_ == ReportingPolicy::kArchive) {
    Archive(pending_report.report_id);
    return true;
  }

  pending_reports_.push_back(pending_report);

  return true;
}

bool Queue::Upload(const Report& report) {
  upload_attempts_[report.Id()]++;
  info_.RecordUploadAttemptNumber(upload_attempts_[report.Id()]);

  std::string server_report_id;
  const auto response = crash_server_->MakeRequest(
      report, snapshot_manager_->GetSnapshot(report.SnapshotUuid()), &server_report_id);

  switch (response) {
    case CrashServer::UploadStatus::kSuccess:
      FX_LOGST(INFO, tags_->Get(report.Id()))
          << "Successfully uploaded report at https://crash.corp.google.com/" << server_report_id;
      info_.MarkReportAsUploaded(server_report_id, upload_attempts_[report.Id()]);
      FreeResources(report);
      return true;
    case CrashServer::UploadStatus::kThrottled:
      info_.MarkReportAsThrottledByServer(upload_attempts_[report.Id()]);
      FreeResources(report);
      return true;
    case CrashServer::UploadStatus::kFailure:
      return false;
  }
}

void Queue::Archive(const ReportId report_id) {
  FX_LOGST(INFO, tags_->Get(report_id)) << "Archiving local report under /tmp/reports";
  info_.MarkReportAsArchived();
  // In Archive mode, the report is never placed in |pending_| nor |upload_attemps_|.
}

void Queue::GarbageCollect(const ReportId report_id) {
  FX_LOGST(INFO, tags_->Get(report_id)) << "Garbage collected local report";
  info_.MarkReportAsGarbageCollected(upload_attempts_[report_id]);
  FreeResources(report_id);
}

void Queue::FreeResources(const ReportId report_id) {
  if (store_.Contains(report_id)) {
    FreeResources(store_.Get(report_id));
    return;
  }

  // The report no longer exists in the store.
  tags_->Unregister(report_id);
  upload_attempts_.erase(report_id);
}

void Queue::FreeResources(const Report& report) {
  snapshot_manager_->Release(report.SnapshotUuid());
  tags_->Unregister(report.Id());
  upload_attempts_.erase(report.Id());
  store_.Remove(report.Id());
}

size_t Queue::UploadAll() {
  std::deque<PendingReport> new_pending_reports;
  for (const auto& pending_report : pending_reports_) {
    // |pending_reports_| is kept in sync with |store_| so Get should only ever fail if the
    // report is deleted from the store by an external influence, e.g., the filesystem flushes
    // /cache.
    if (!store_.Contains(pending_report.report_id)) {
      FreeResources(pending_report.report_id);
      continue;
    }

    if (!Upload(store_.Get(pending_report.report_id))) {
      new_pending_reports.push_back(pending_report);
    }
  }

  pending_reports_.swap(new_pending_reports);

  // |new_pending_reports| now contains the pending reports before attempting to upload them.
  return new_pending_reports.size() - pending_reports_.size();
}

void Queue::Delete(const ReportId report_id) {
  info_.MarkReportAsDeleted(upload_attempts_[report_id]);
  FreeResources(report_id);
}

void Queue::DeleteAll() {
  FX_LOGS(INFO) << fxl::StringPrintf("Deleting all %zu pending reports", Size());
  for (const auto& [report_id, _] : pending_reports_) {
    Delete(report_id);
  }
  pending_reports_.clear();

  store_.RemoveAll();
}

// The queue is inheritly conservative with uploading crash reports meaning that a report that is
// forbidden from being uploaded will never be uploaded while crash reports that are permitted to
// be uploaded may later be considered to be forbidden. This is due to the fact that when uploads
// are disabled all reports are immediately archived after having been added to the queue, thus we
// never have to worry that a report that shouldn't be uploaded ends up being uploaded when the
// reporting policy changes.
void Queue::WatchReportingPolicy(ReportingPolicyWatcher* watcher) {
  auto OnReportingPolicyChange = [this](const ReportingPolicy policy) {
    reporting_policy_ = policy;
    switch (reporting_policy_) {
      case ReportingPolicy::kDoNotFileAndDelete:
        upload_all_every_fifteen_minutes_task_.Cancel();
        DeleteAll();
        break;
      case ReportingPolicy::kUpload:
        UploadAllEveryFifteenMinutes();
        break;
      case ReportingPolicy::kArchive:
        // The reporting policy shouldn't change to Archive outside of tests.
        break;
      case ReportingPolicy::kUndecided:
        upload_all_every_fifteen_minutes_task_.Cancel();
        break;
    }
  };

  OnReportingPolicyChange(watcher->CurrentPolicy());
  watcher->OnPolicyChange([=](const ReportingPolicy policy) { OnReportingPolicyChange(policy); });
}

void Queue::WatchNetwork(NetworkWatcher* network_watcher) {
  network_watcher->Register([this](const bool network_is_reachable) {
    if (!stop_uploading_ && network_is_reachable) {
      // Save the size of |pending_reports_| because UploadAll mutates |pending_reports_|.
      if (const auto pending = Size();
          reporting_policy_ == ReportingPolicy::kUpload && pending > 0) {
        const auto uploaded = UploadAll();
        if (uploaded > 0) {
          FX_LOGS(INFO) << fxl::StringPrintf(
              "Successfully uploaded %zu of %zu pending crash reports on network reachable ",
              uploaded, pending);
        } else {
          FX_LOGS(INFO) << fxl::StringPrintf(
              "Failed to upload any of the %zu pending crash reports on network reachable ",
              pending);
        }
      }
    }
  });
}

void Queue::UploadAllEveryFifteenMinutes() {
  if (stop_uploading_) {
    return;
  }

  // Save the size of |pending_reports_| because UploadAll mutates |pending_reports_|.
  if (const auto pending = Size(); reporting_policy_ == ReportingPolicy::kUpload && pending > 0) {
    const auto uploaded = UploadAll();
    if (uploaded > 0) {
      FX_LOGS(INFO) << fxl::StringPrintf(
          "Successfully uploaded %zu of %zu pending crash reports as part of the "
          "15-minute periodic upload",
          uploaded, pending);
    } else {
      FX_LOGS(INFO) << fxl::StringPrintf(
          "Failed to upload any of the %zu pending crash reports as part of the "
          "15-minute periodic upload",
          pending);
    }
  }
  if (const auto status =
          upload_all_every_fifteen_minutes_task_.PostDelayed(dispatcher_, zx::min(15));
      status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Error posting periodic upload task to async loop. Won't retry.";
  }
}

}  // namespace crash_reports
}  // namespace forensics
