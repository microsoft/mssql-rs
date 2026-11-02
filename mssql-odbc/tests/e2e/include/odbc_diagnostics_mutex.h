// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#pragma once

#include <condition_variable>
#include <cstddef>
#include <mutex>

// SharedStmt observers may overlap, but must stop admitting readers as soon
// as an owner queues exclusive diagnostic cleanup. std::shared_mutex does not
// guarantee this, so a busy observer loop can starve idle cancellation.
class OdbcDiagnosticsMutex {
public:
    void lock() {
        std::unique_lock<std::mutex> lock(mutex_);
        ++waiting_writers_;
        changed_.wait(lock, [&] { return !writing_ && readers_ == 0; });
        --waiting_writers_;
        writing_ = true;
    }

    void unlock() {
        std::lock_guard<std::mutex> lock(mutex_);
        writing_ = false;
        changed_.notify_all();
    }

    void lock_shared() {
        std::unique_lock<std::mutex> lock(mutex_);
        changed_.wait(lock, [&] { return !writing_ && waiting_writers_ == 0; });
        ++readers_;
    }

    bool try_lock_shared() {
        std::lock_guard<std::mutex> lock(mutex_);
        if (writing_ || waiting_writers_ != 0) {
            return false;
        }
        ++readers_;
        return true;
    }

    void unlock_shared() {
        std::lock_guard<std::mutex> lock(mutex_);
        if (--readers_ == 0) {
            changed_.notify_all();
        }
    }

private:
    std::mutex mutex_;
    std::condition_variable changed_;
    std::size_t readers_ = 0;
    std::size_t waiting_writers_ = 0;
    bool writing_ = false;
};
