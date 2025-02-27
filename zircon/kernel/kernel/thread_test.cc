// Copyright 2016, 2018 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <debug.h>
#include <lib/fit/defer.h>
#include <lib/unittest/unittest.h>
#include <lib/zircon-internal/macros.h>
#include <pow2.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <fbl/array.h>
#include <kernel/auto_lock.h>
#include <kernel/auto_preempt_disabler.h>
#include <kernel/cpu.h>
#include <kernel/event.h>
#include <kernel/mp.h>
#include <kernel/mutex.h>
#include <kernel/percpu.h>
#include <kernel/thread.h>
#include <ktl/array.h>
#include <ktl/atomic.h>
#include <ktl/bit.h>
#include <ktl/unique_ptr.h>

#include "zircon/time.h"

namespace {

// Wait for condition |cond| to become true, polling with slow exponential backoff
// to avoid pegging the CPU.
template <typename F>
void wait_for_cond(F cond) {
  zx_duration_t wait_duration = ZX_USEC(1);
  while (!cond()) {
    Thread::Current::SleepRelative(wait_duration);
    // Increase wait_duration time by ~10%.
    wait_duration += (wait_duration / 10u + 1u);
  }
}

// Create and manage a spinning thread.
class WorkerThread {
 public:
  explicit WorkerThread(const char* name) {
    thread_ = Thread::Create(name, &WorkerThread::WorkerBody, this, LOW_PRIORITY);
    ASSERT(thread_ != nullptr);
  }

  ~WorkerThread() { Join(); }

  void Start() { thread_->Resume(); }

  void Join() {
    if (thread_ != nullptr) {
      worker_should_stop_.store(1);
      int unused_retcode;
      zx_status_t result = thread_->Join(&unused_retcode, ZX_TIME_INFINITE);
      ASSERT(result == ZX_OK);
      thread_ = nullptr;
    }
  }

  void WaitForWorkerProgress() {
    int start_iterations = worker_iterations();
    wait_for_cond([start_iterations, this]() { return start_iterations != worker_iterations(); });
  }

  Thread* thread() const { return thread_; }

  int worker_iterations() { return worker_iterations_.load(); }

  DISALLOW_COPY_ASSIGN_AND_MOVE(WorkerThread);

 private:
  static int WorkerBody(void* arg) {
    auto* self = reinterpret_cast<WorkerThread*>(arg);
    while (self->worker_should_stop_.load() == 0) {
      self->worker_iterations_.fetch_add(1);
    }
    return 0;
  }

  ktl::atomic<int> worker_iterations_{0};
  ktl::atomic<int> worker_should_stop_{0};
  Thread* thread_;
};

bool set_affinity_self_test() {
  BEGIN_TEST;

  // Our worker thread will attempt to schedule itself onto each core, one at
  // a time, and ensure it landed in the right location.
  cpu_mask_t online_cpus = mp_get_online_mask();
  ASSERT_NE(online_cpus, 0u, "Expected at least one CPU to be online.");
  auto worker_body = +[](void* arg) -> int {
    cpu_mask_t& online_cpus = *reinterpret_cast<cpu_mask_t*>(arg);
    Thread* const self = Thread::Current::Get();

    for (cpu_num_t c = 0u; c <= highest_cpu_set(online_cpus); c++) {
      // Skip offline CPUs.
      if ((cpu_num_to_mask(c) & online_cpus) == 0) {
        continue;
      }

      // Set affinity to the given core.
      self->SetCpuAffinity(cpu_num_to_mask(c));

      // Ensure we are on the correct CPU.
      const cpu_num_t current_cpu = arch_curr_cpu_num();
      if (current_cpu != c) {
        UNITTEST_FAIL_TRACEF("Expected to be running on CPU %u, but actually running on %u.", c,
                             current_cpu);
        return ZX_ERR_INTERNAL;
      }
    }

    return ZX_OK;
  };
  Thread* worker =
      Thread::Create("set_affinity_self_test_worker", worker_body, &online_cpus, DEFAULT_PRIORITY);
  ASSERT_NONNULL(worker, "thread_create failed.");
  worker->Resume();

  // Wait for the worker thread to test itself.
  int worker_retcode;
  ASSERT_EQ(worker->Join(&worker_retcode, ZX_TIME_INFINITE), ZX_OK, "Failed to join thread.");
  EXPECT_EQ(worker_retcode, ZX_OK, "Worker thread failed.");

  END_TEST;
}

bool set_affinity_other_test() {
  BEGIN_TEST;

  struct WorkerState {
    ktl::atomic<cpu_num_t> current_cpu{INVALID_CPU};
    ktl::atomic<int> should_stop{0};
  } state;

  // Start a worker, which reports the CPU it is running on.
  auto worker_body = [](void* arg) -> int {
    WorkerState& state = *reinterpret_cast<WorkerState*>(arg);
    while (state.should_stop.load() != 1) {
      state.current_cpu.store(arch_curr_cpu_num());
    }
    return 0;
  };
  Thread* worker =
      Thread::Create("set_affinity_other_test_worker", worker_body, &state, LOW_PRIORITY);
  worker->Resume();

  // Migrate the worker task amongst different threads.
  const cpu_mask_t online_cpus = mp_get_online_mask();
  ASSERT_NE(online_cpus, 0u, "Expected at least one CPU to be online.");
  for (cpu_num_t c = 0u; c <= highest_cpu_set(online_cpus); c++) {
    // Skip offline CPUs.
    if ((cpu_num_to_mask(c) & online_cpus) == 0) {
      continue;
    }

    // Set affinity to the given core.
    worker->SetCpuAffinity(cpu_num_to_mask(c));

    // Wait for it to land on the correct CPU.
    wait_for_cond([c, &state]() { return state.current_cpu.load() == c; });
  }

  // Done.
  state.should_stop.store(1);
  int worker_retcode;
  ASSERT_EQ(worker->Join(&worker_retcode, ZX_TIME_INFINITE), ZX_OK, "Failed to join thread.");

  END_TEST;
}

bool thread_last_cpu_new_thread() {
  BEGIN_TEST;

  // Create a worker, but don't start it.
  WorkerThread worker("last_cpu_new_thread");

  // Ensure we get INVALID_CPU as last cpu.
  ASSERT_EQ(worker.thread()->LastCpu(), INVALID_CPU, "Last CPU on unstarted thread invalid.");

  // Clean up the thread.
  worker.Start();
  worker.Join();

  END_TEST;
}

bool thread_last_cpu_running_thread() {
  BEGIN_TEST;

  WorkerThread worker("last_cpu_running_thread");
  worker.Start();

  // Migrate the worker task across different CPUs.
  const cpu_mask_t online_cpus = mp_get_online_mask();
  ASSERT_NE(online_cpus, 0u, "Expected at least one CPU to be online.");
  for (cpu_num_t c = 0u; c <= highest_cpu_set(online_cpus); c++) {
    // Skip offline CPUs.
    if ((cpu_num_to_mask(c) & online_cpus) == 0) {
      continue;
    }

    // Set affinity to the given core.
    worker.thread()->SetCpuAffinity(cpu_num_to_mask(c));

    // Ensure it is reported at the correct CPU.
    wait_for_cond([c, &worker]() { return worker.thread()->LastCpu() == c; });
  }

  END_TEST;
}

bool thread_empty_soft_affinity_mask() {
  BEGIN_TEST;

  WorkerThread worker("empty_soft_affinity_mask");
  worker.Start();

  // Wait for the thread to start running.
  worker.WaitForWorkerProgress();

  // Set affinity to an invalid (empty) mask.
  worker.thread()->SetSoftCpuAffinity(0);

  // Ensure that the thread is still running.
  worker.WaitForWorkerProgress();

  END_TEST;
}

bool thread_conflicting_soft_and_hard_affinity() {
  BEGIN_TEST;

  // Find two different CPUs to run our tests on.
  cpu_mask_t online = mp_get_online_mask();
  ASSERT_TRUE(online != 0, "No CPUs online.");
  cpu_num_t a = highest_cpu_set(online);
  cpu_num_t b = lowest_cpu_set(online);
  if (a == b) {
    // Skip test on single CPU machines.
    unittest_printf("Only 1 CPU active in this machine. Skipping test.\n");
    return true;
  }

  WorkerThread worker("conflicting_soft_and_hard_affinity");
  worker.Start();

  // Set soft affinity to CPU A, wait for the thread to start running there.
  worker.thread()->SetSoftCpuAffinity(cpu_num_to_mask(a));
  wait_for_cond([&worker, a]() { return worker.thread()->LastCpu() == a; });

  // Set hard affinity to CPU B, ensure the thread migrates there.
  worker.thread()->SetCpuAffinity(cpu_num_to_mask(b));
  wait_for_cond([&worker, b]() { return worker.thread()->LastCpu() == b; });

  // Remove the hard affinity. Make sure the thread migrates back to CPU A.
  worker.thread()->SetCpuAffinity(CPU_MASK_ALL);
  wait_for_cond([&worker, a]() { return worker.thread()->LastCpu() == a; });

  END_TEST;
}

bool set_migrate_fn_test() {
  BEGIN_TEST;

  cpu_mask_t active_cpus = mp_get_active_mask();
  if (active_cpus == 0 || ispow2(active_cpus)) {
    printf("Expected multiple CPUs to be active.\n");
    return true;
  }

  // The worker thread will attempt to migrate to another CPU.
  auto worker_body = [](void* arg) -> int {
    cpu_num_t current_cpu = arch_curr_cpu_num();
    Thread* self = Thread::Current::Get();
    self->SetCpuAffinity(mp_get_active_mask() ^ cpu_num_to_mask(current_cpu));

    cpu_num_t target_cpu = arch_curr_cpu_num();
    if (current_cpu == target_cpu) {
      UNITTEST_FAIL_TRACEF("Expected to be running on CPU %u, but actually running on %u.",
                           target_cpu, current_cpu);
      return ZX_ERR_INTERNAL;
    }

    return ZX_OK;
  };
  Thread* worker =
      Thread::Create("set_migrate_fn_test_worker", worker_body, nullptr, DEFAULT_PRIORITY);
  ASSERT_NONNULL(worker, "thread_create failed.");

  // Set the migrate function, and begin execution.
  struct {
    int count = 0;
    cpu_num_t last_cpu = INVALID_CPU;
    Thread::MigrateStage next_stage = Thread::MigrateStage::Before;
    bool success = true;
  } migrate_state;
  worker->SetMigrateFn([&migrate_state](Thread* thread, Thread::MigrateStage stage) {
    ++migrate_state.count;

    cpu_num_t current_cpu = arch_curr_cpu_num();
    if ((stage == Thread::MigrateStage::After) && (migrate_state.last_cpu == current_cpu)) {
      UNITTEST_FAIL_TRACEF("Expected to have migrated CPU.");
      migrate_state.success = false;
    }
    migrate_state.last_cpu = current_cpu;

    if (migrate_state.next_stage != stage) {
      UNITTEST_FAIL_TRACEF("Expected migrate stage %d, but received migrate stage %d.",
                           static_cast<int>(migrate_state.next_stage), static_cast<int>(stage));
      migrate_state.success = false;
    }

    switch (migrate_state.next_stage) {
      case Thread::MigrateStage::Before:
        migrate_state.next_stage = Thread::MigrateStage::After;
        break;
      case Thread::MigrateStage::After:
        migrate_state.next_stage = Thread::MigrateStage::Exiting;
        if (thread->LastCpuLocked() != current_cpu) {
          UNITTEST_FAIL_TRACEF("Expected last CPU to be current CPU after migrate.");
          migrate_state.success = false;
        }
        break;
      case Thread::MigrateStage::Exiting:
        break;
    }

    if (!thread_lock.IsHeld()) {
      UNITTEST_FAIL_TRACEF("Expected the thread lock to be held.");
      migrate_state.success = false;
    }
  });
  worker->Resume();

  // Wait for the worker thread to test itself.
  int worker_retcode;
  ASSERT_EQ(worker->Join(&worker_retcode, ZX_TIME_INFINITE), ZX_OK, "Failed to join thread.");
  EXPECT_EQ(worker_retcode, ZX_OK, "Worker thread failed.");
  EXPECT_EQ(migrate_state.count, 3, "Migrate function was not called 3 times.");
  EXPECT_TRUE(migrate_state.success, "Migrate function was not called with the expected state.")

  END_TEST;
}

bool set_migrate_ready_threads_test() {
  BEGIN_TEST;

  cpu_mask_t active_cpus = mp_get_active_mask();
  if (active_cpus == 0 || ispow2(active_cpus)) {
    printf("Expected multiple CPUs to be active.\n");
    return true;
  }

  const cpu_num_t kStartingCpu = 0;
  const cpu_num_t kTargetCpu = 1;

  // The worker thread will validate that it is running on the target CPU.
  const thread_start_routine worker_body = [](void* arg) -> int {
    const cpu_num_t current_cpu = arch_curr_cpu_num();
    if (current_cpu != kTargetCpu) {
      UNITTEST_FAIL_TRACEF("Expected to be running on CPU %u, but actually running on %u.",
                           kTargetCpu, current_cpu);
      return ZX_ERR_INTERNAL;
    }
    return ZX_OK;
  };

  ktl::array<Thread*, 4> workers{nullptr, nullptr, nullptr, nullptr};

  for (auto& worker : workers) {
    worker = Thread::Create("set_migrate_ready_threads_test_worker", worker_body, nullptr,
                            DEFAULT_PRIORITY);
    ASSERT_NONNULL(worker, "thread_create failed.");
    worker->SetCpuAffinity(cpu_num_to_mask(kStartingCpu));
  }

  // Move the test thread to the same CPU that the workers will start on.
  Thread* const current_thread = Thread::Current::Get();
  cpu_mask_t original_affinity = current_thread->GetCpuAffinity();
  current_thread->SetCpuAffinity(cpu_num_to_mask(kStartingCpu));
  ASSERT_EQ(arch_curr_cpu_num(), kStartingCpu, "Failed to move test thread to the starting CPU.");

  auto cleanup = fit::defer([current_thread, original_affinity]() {
    // Restore original CPU affinity of the test thread.
    current_thread->SetCpuAffinity(original_affinity);
  });

  {
    AutoPreemptDisabler preempt_disabled_guard;
    const auto context_switches_before = percpu::GetCurrent().stats.context_switches;

    // Resume the workers with preemption disabled. The workers should stack up
    // behind the current thread in the run queue. BE CAREFUL not to do anything
    // that would block until the workers are validated.
    for (Thread* worker : workers) {
      worker->Resume();

      // Validate the thread state.
      thread_state state;
      cpu_num_t curr_cpu;
      {
        Guard<MonitoredSpinLock, IrqSave> guard{ThreadLock::Get(), SOURCE_TAG};
        state = worker->state();
        curr_cpu = worker->scheduler_state().curr_cpu();
      }
      ASSERT_EQ(state, THREAD_READY, "The worker was in the wrong state.");
      ASSERT_EQ(curr_cpu, kStartingCpu, "The worker was assigned to the wrong CPU.");
    }

    // Migrate the ready threads to a different CPU. BE CAREFUL not to do
    // anything that would block until the workers are migrated.
    for (Thread* worker : workers) {
      worker->SetCpuAffinity(cpu_num_to_mask(kTargetCpu));
    }

    const auto context_switches_after = percpu::GetCurrent().stats.context_switches;
    ASSERT_EQ(context_switches_before, context_switches_after,
              "The test thread context switched during the critical section.");
  }

  // Wait for the worker thread results.
  for (Thread* worker : workers) {
    int worker_retcode;
    ASSERT_EQ(worker->Join(&worker_retcode, ZX_TIME_INFINITE), ZX_OK, "Failed to join thread.");
    EXPECT_EQ(worker_retcode, ZX_OK, "Worker thread failed.");
  }

  END_TEST;
}

bool migrate_unpinned_threads_test() {
  BEGIN_TEST;

  cpu_mask_t active_cpus = mp_get_active_mask();
  if (active_cpus == 0 || ispow2(active_cpus)) {
    printf("Expected multiple CPUs to be active.\n");
    return true;
  }

  const cpu_num_t kStartingCpu = 1;
  struct Events {
    AutounsignalEvent worker_started;
    AutounsignalEvent event;
  } events;

  // Setup the thread that will be migrated.
  auto worker_body = [](void* arg) -> int {
    auto events = static_cast<Events*>(arg);
    events->worker_started.Signal();
    events->event.Wait();
    return 0;
  };
  AutounsignalEvent* const event = &events.event;
  auto fn = [event](Thread* thread, Thread::MigrateStage stage) {
    thread_lock.AssertHeld();
    event->SignalLocked();
  };
  Thread* worker = Thread::Create("worker", worker_body, &events, DEFAULT_PRIORITY);
  worker->SetSoftCpuAffinity(cpu_num_to_mask(kStartingCpu));
  worker->SetMigrateFn(fn);
  worker->Resume();

  events.worker_started.Wait();

  // Setup the thread that will perform the migration.
  auto migrate_body = []() TA_REQ(thread_lock) __NO_RETURN {
    Scheduler::MigrateUnpinnedThreads();
    mp_set_curr_cpu_active(true);
    thread_lock.Release();
    Thread::Current::Exit(0);
  };
  Thread* migrate =
      Thread::CreateEtc(nullptr, "migrate", nullptr, nullptr, DEFAULT_PRIORITY, migrate_body);
  migrate->SetCpuAffinity(cpu_num_to_mask(kStartingCpu));
  migrate->Resume();

  int retcode;
  ASSERT_EQ(migrate->Join(&retcode, ZX_TIME_INFINITE), ZX_OK, "Failed to join migrate thread.");
  EXPECT_EQ(retcode, ZX_OK, "Migrate thread failed.");
  ASSERT_EQ(worker->Join(&retcode, ZX_TIME_INFINITE), ZX_OK, "Failed to join worker thread.");
  EXPECT_EQ(retcode, ZX_OK, "Worker thread failed.");

  END_TEST;
}

bool runtime_test() {
  BEGIN_TEST;
  const zx_duration_t kCpuTime = 10, kQueueTime = 20;
  Thread::RuntimeStats stats;
  EXPECT_EQ(thread_state::THREAD_INITIAL, stats.GetSchedulerStats().state);
  EXPECT_EQ(0, stats.GetSchedulerStats().state_time);
  EXPECT_EQ(0, stats.GetSchedulerStats().cpu_time);
  EXPECT_EQ(0, stats.GetSchedulerStats().queue_time);
  EXPECT_EQ(0, stats.TotalRuntime().page_fault_ticks);
  EXPECT_EQ(0, stats.TotalRuntime().lock_contention_ticks);

  // Test that runtime is calculated as a function of the stats and the current time spent queued.
  // When the state is set to THREAD_READY, TotalRuntime calculates queue_time as:
  //   runtime.queue_time + (current_time() - state_time)
  //
  // We subtract 1 from current time to ensure that the difference between the actual current_time
  // and state_time is nonzero.
  stats.UpdateSchedulerStats({
      .state = thread_state::THREAD_READY,
      .state_time = current_time() - 1,
      .cpu_time = kCpuTime,
  });
  EXPECT_EQ(thread_state::THREAD_READY, stats.GetSchedulerStats().state);
  EXPECT_NE(0, stats.GetSchedulerStats().state_time);
  EXPECT_EQ(kCpuTime, stats.GetSchedulerStats().cpu_time);
  EXPECT_EQ(0, stats.GetSchedulerStats().queue_time);
  // Ensure queue time includes current time spent in queue, and cpu time does not.
  TaskRuntimeStats runtime = stats.TotalRuntime();
  EXPECT_NE(0, runtime.queue_time);
  EXPECT_EQ(kCpuTime, runtime.cpu_time);
  EXPECT_EQ(0, runtime.page_fault_ticks);
  EXPECT_EQ(0, runtime.lock_contention_ticks);

  // Test that runtime is calculated as a function of the stats and the current time spent running.
  // When the state is set to THREAD_RUNNING, TotalRuntime calculates cpu_time as:
  //   runtime.cpu_time + (current_time() - state_time)
  //
  // We subtract 1 from current time to ensure that the difference between the actual current_time
  // and state_time is nonzero.
  stats.UpdateSchedulerStats({
      .state = thread_state::THREAD_RUNNING,
      .state_time = current_time() - 1,
      .queue_time = kQueueTime,
  });
  EXPECT_EQ(thread_state::THREAD_RUNNING, stats.GetSchedulerStats().state);
  EXPECT_NE(0, stats.GetSchedulerStats().state_time);
  EXPECT_EQ(kCpuTime, stats.GetSchedulerStats().cpu_time);
  EXPECT_EQ(kQueueTime, stats.GetSchedulerStats().queue_time);
  // Ensure cpu time includes current time, and queue time does not.
  runtime = stats.TotalRuntime();
  EXPECT_NE(kCpuTime, runtime.cpu_time);
  EXPECT_EQ(kQueueTime, runtime.queue_time);
  EXPECT_EQ(0, runtime.page_fault_ticks);
  EXPECT_EQ(0, runtime.lock_contention_ticks);

  // Add other times.
  const zx_ticks_t kPageFaultTicks = 30;
  const zx_ticks_t kLockContentionTicks = 40;

  stats.AddPageFaultTicks(kPageFaultTicks);
  runtime = stats.TotalRuntime();
  EXPECT_NE(kCpuTime, runtime.cpu_time);
  EXPECT_EQ(kQueueTime, runtime.queue_time);
  EXPECT_EQ(kPageFaultTicks, runtime.page_fault_ticks);
  EXPECT_EQ(0, runtime.lock_contention_ticks);

  stats.AddLockContentionTicks(kLockContentionTicks);
  runtime = stats.TotalRuntime();
  EXPECT_NE(kCpuTime, runtime.cpu_time);
  EXPECT_EQ(kQueueTime, runtime.queue_time);
  EXPECT_EQ(kPageFaultTicks, runtime.page_fault_ticks);
  EXPECT_EQ(kLockContentionTicks, runtime.lock_contention_ticks);

  END_TEST;
}

bool migrate_stress_test() {
  BEGIN_TEST;

  // Get number of CPUs in the system.
  int active_cpus = ktl::popcount(mp_get_active_mask());
  printf("Found %d active CPU(s)\n", active_cpus);
  if (active_cpus <= 1) {
    printf("Test can only proceed with multiple active CPUs.\n");
    return true;
  }

  // The worker thread body.
  //
  // Each thread spins ensuring that it is only running on a single, particular
  // CPU. The migration function below updates which CPU the thread is allowed
  // to run on.
  struct ThreadState {
    Thread* thread;
    ktl::atomic<bool> should_stop = false;
    ktl::atomic<bool> started = false;

    // Prevent reporting lock violations inside the migration function to avoid reentering the
    // scheduler and interfering with the migration function state.
    // TODO(fxbug.dev/77329): Figure out how to support violation reporting in sensitive code paths.
    DECLARE_SPINLOCK(ThreadState, lockdep::LockFlagsReportingDisabled) lock;

    cpu_num_t allowed_cpu TA_GUARDED(lock) = 0;
    bool is_migrating TA_GUARDED(lock) = false;
  };

  auto worker_body = +[](void* arg) -> int {
    ThreadState* state = static_cast<ThreadState*>(arg);
    state->started = true;

    while (!state->should_stop.load(ktl::memory_order_relaxed)) {
      {
        Guard<SpinLock, IrqSave> guard(&state->lock);
        ZX_ASSERT_MSG(!state->is_migrating,
                      "Worker thread scheduled between MigrationStage::Before and "
                      "MigrationStage::After being called.\n");
        ZX_ASSERT_MSG(arch_curr_cpu_num() == state->allowed_cpu,
                      "Worker thread running on unexpected CPU: expected = %d, running on = %d\n",
                      state->allowed_cpu, arch_curr_cpu_num());
      }
      Thread::Current::Yield();
    }
    return 0;
  };

  // CPU that all threads start running on. Can be any valid CPU.
  const cpu_num_t starting_cpu = arch_curr_cpu_num();

  // Create threads.
  const int num_threads = active_cpus;
  fbl::AllocChecker ac;
  fbl::Array<ThreadState> threads(new (&ac) ThreadState[num_threads], num_threads);
  ZX_ASSERT(ac.check());
  for (int i = 0; i < num_threads; i++) {
    // Create the thread.
    threads[i].thread =
        Thread::Create("migrate_stress_test worker", worker_body, &threads[i], DEFAULT_PRIORITY);
    ASSERT_NONNULL(threads[i].thread, "Thread::Create failed.");
    {
      Guard<SpinLock, IrqSave> guard(&threads[i].lock);
      threads[i].allowed_cpu = starting_cpu;
    }

    auto migrate_fn = [&state = threads[i]](Thread* thread, Thread::MigrateStage stage) {
      Guard<SpinLock, IrqSave> guard(&state.lock);
      switch (stage) {
        case Thread::MigrateStage::Before:
          ZX_ASSERT_MSG(!state.is_migrating, "Thread is already migrating");
          ZX_ASSERT_MSG(state.allowed_cpu == arch_curr_cpu_num(),
                        "Migrate function called on incorrect CPU.");
          state.allowed_cpu = -1;
          state.is_migrating = true;
          break;

        case Thread::MigrateStage::After:
          ZX_ASSERT(state.is_migrating);
          state.is_migrating = false;
          state.allowed_cpu = arch_curr_cpu_num();
          break;

        case Thread::MigrateStage::Exiting:
          break;
      }
    };

    // Set the migration function to ensure that `allowed_cpu` keeps up to date.
    threads[i].thread->SetMigrateFn(migrate_fn);

    // Start running on `starting_cpu`.
    threads[i].thread->SetSoftCpuAffinity(1 << starting_cpu);
    threads[i].thread->Resume();
  }

  // Wait for the worker threads to execute at least once to ensure the migrate function gets
  // called. Threads that have never executed are moved without calling the migrate function, which
  // could cause this test to incorrectly assert in the worker loop.
  for (const auto& thread_state : threads) {
    while (!thread_state.started) {
      Thread::Current::SleepRelative(ZX_USEC(100));
    }
  }

  // Mutate threads as they run.
  for (int i = 0; i < 100000; i++) {
    for (size_t j = 0; j < threads.size(); j++) {
      const cpu_mask_t affinity = (i + j) & mp_get_active_mask();
      if (affinity) {
        threads[j].thread->SetSoftCpuAffinity(affinity);
      }
    }
    Thread::Current::Yield();
  }

  // Join threads.
  for (auto& thread : threads) {
    thread.should_stop.store(true);
    int ret;
    thread.thread->Join(&ret, ZX_TIME_INFINITE);
  }

  END_TEST;
}

bool backtrace_test() {
  BEGIN_TEST;

  char buffer[64]{};

  // See that we don't write more than the specified length.
  EXPECT_EQ(Thread::Current::AppendBacktrace(buffer, 1), 1U);
  EXPECT_EQ(0, buffer[1]);

  // See that we can generate a backtrace.  If this fails, then perhaps the code is compiled without
  // frame pointers?
  memset(buffer, 0, sizeof(buffer));
  EXPECT_GT(Thread::Current::AppendBacktrace(buffer, sizeof(buffer) - 1), 0U);
  EXPECT_NE(nullptr, strstr(buffer, "{{{bt:0:"));

  END_TEST;
}

bool scoped_allocation_disabled_test() {
  BEGIN_TEST;

  EXPECT_TRUE(Thread::Current::memory_allocation_state().IsEnabled());

  Thread::Current::memory_allocation_state().Disable();
  EXPECT_FALSE(Thread::Current::memory_allocation_state().IsEnabled());
  Thread::Current::memory_allocation_state().Enable();
  EXPECT_TRUE(Thread::Current::memory_allocation_state().IsEnabled());

  {
    EXPECT_TRUE(Thread::Current::memory_allocation_state().IsEnabled());
    {
      ScopedMemoryAllocationDisabled smad;
      EXPECT_FALSE(Thread::Current::memory_allocation_state().IsEnabled());
      {
        ScopedMemoryAllocationDisabled smad;
        EXPECT_FALSE(Thread::Current::memory_allocation_state().IsEnabled());
      }
      EXPECT_FALSE(Thread::Current::memory_allocation_state().IsEnabled());
    }
    EXPECT_TRUE(Thread::Current::memory_allocation_state().IsEnabled());
  }

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(thread_tests)
UNITTEST("set_affinity_self_test", set_affinity_self_test)
UNITTEST("set_affinity_other_test", set_affinity_other_test)
UNITTEST("thread_last_cpu_new_thread", thread_last_cpu_new_thread)
UNITTEST("thread_last_cpu_running_thread", thread_last_cpu_running_thread)
UNITTEST("thread_empty_soft_affinity_mask", thread_empty_soft_affinity_mask)
UNITTEST("thread_conflicting_soft_and_hard_affinity", thread_conflicting_soft_and_hard_affinity)
UNITTEST("set_migrate_fn_test", set_migrate_fn_test)
UNITTEST("set_migrate_ready_threads_test", set_migrate_ready_threads_test)
UNITTEST("migrate_unpinned_threads_test", migrate_unpinned_threads_test)
UNITTEST("migrate_stress_test", migrate_stress_test)
UNITTEST("runtime_test", runtime_test)
UNITTEST("backtrace_test", backtrace_test)
UNITTEST("scoped_allocation_disabled_test", scoped_allocation_disabled_test)
UNITTEST_END_TESTCASE(thread_tests, "thread", "thread tests")
