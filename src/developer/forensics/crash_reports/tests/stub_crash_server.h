// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_TESTS_STUB_CRASH_SERVER_H_
#define SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_TESTS_STUB_CRASH_SERVER_H_

#include <map>
#include <string>
#include <vector>

#include "src/developer/forensics/crash_reports/annotation_map.h"
#include "src/developer/forensics/crash_reports/crash_server.h"

namespace forensics {
namespace crash_reports {

extern const char kStubCrashServerUrl[];
extern const char kStubServerReportId[];

class StubCrashServer : public CrashServer {
 public:
  explicit StubCrashServer(const std::vector<CrashServer::UploadStatus>& request_return_values)
      : CrashServer(nullptr, kStubCrashServerUrl, nullptr),
        request_return_values_(request_return_values) {
    next_return_value_ = request_return_values_.cbegin();
  }

  ~StubCrashServer();

  CrashServer::UploadStatus MakeRequest(const Report& report, Snapshot snapshot,
                                        std::string* server_report_id) override;

  // Whether the crash server expects at least one more call to MakeRequest().
  bool ExpectRequest() { return next_return_value_ != request_return_values_.cend(); }

  // Returns the annotations that were passed to the latest MakeRequest() call.
  const AnnotationMap& latest_annotations() { return latest_annotations_; }

  // Returns the keys for the attachments that were passed to the latest MakeRequest() call.
  const std::vector<std::string>& latest_attachment_keys() { return latest_attachment_keys_; }

 private:
  const std::vector<CrashServer::UploadStatus> request_return_values_;
  std::vector<CrashServer::UploadStatus>::const_iterator next_return_value_;

  AnnotationMap latest_annotations_;
  std::vector<std::string> latest_attachment_keys_;
};

}  // namespace crash_reports
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_TESTS_STUB_CRASH_SERVER_H_
