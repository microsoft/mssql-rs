// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"

#include <atomic>
#include <future>
#include <thread>

namespace {

// Exercise the fixture's actual gate without allocating handles or connecting.
class OdbcDiagnosticsMutexTest : public ODBCTest {
protected:
    void SetUp() override {}
    void TearDown() override {}
};

using namespace std::chrono_literals;

TEST_F(OdbcDiagnosticsMutexTest, SharedReadersOverlap) {
    std::shared_lock<OdbcDiagnosticsMutex> first(diagnostics_mutex_);
    std::promise<void> entered;
    auto ready = entered.get_future();
    std::thread reader([&] {
        std::shared_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
        entered.set_value();
    });
    const auto status = ready.wait_for(3s);
    first.unlock();
    reader.join();
    EXPECT_EQ(std::future_status::ready, status);
}

TEST_F(OdbcDiagnosticsMutexTest, WaitingWriterBlocksLaterReaders) {
    std::promise<void> first_entered;
    auto first_ready = first_entered.get_future();
    std::promise<void> release_first;
    auto release_first_reader = release_first.get_future();
    std::thread first([&] {
        std::shared_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
        first_entered.set_value();
        release_first_reader.wait();
    });
    first_ready.wait();
    std::promise<void> entered;
    auto writer_ready = entered.get_future();
    std::promise<void> release;
    auto release_writer = release.get_future();
    std::thread writer([&] {
        std::unique_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
        entered.set_value();
        release_writer.wait();
    });

    // The first reader prevents exclusive ownership. A refused shared lock
    // therefore proves the writer has registered, without a scheduling sleep.
    bool writer_pending = false;
    const auto deadline = std::chrono::steady_clock::now() + 3s;
    while (std::chrono::steady_clock::now() < deadline) {
        if (!diagnostics_mutex_.try_lock_shared()) {
            writer_pending = true;
            break;
        }
        diagnostics_mutex_.unlock_shared();
        std::this_thread::yield();
    }
    EXPECT_TRUE(writer_pending);

    std::atomic<bool> reader_entered{false};
    std::thread reader([&] {
        std::shared_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
        reader_entered.store(true);
    });
    release_first.set_value();
    first.join();
    EXPECT_EQ(std::future_status::ready, writer_ready.wait_for(3s));
    EXPECT_FALSE(reader_entered.load());
    {
        std::shared_lock<OdbcDiagnosticsMutex> probe(diagnostics_mutex_, std::try_to_lock);
        EXPECT_FALSE(probe.owns_lock());
    }
    release.set_value();
    writer.join();
    reader.join();
    EXPECT_TRUE(reader_entered.load());
}

TEST_F(OdbcDiagnosticsMutexTest, ExclusiveOwnersDoNotOverlap) {
    std::unique_lock<OdbcDiagnosticsMutex> first(diagnostics_mutex_);
    std::promise<void> entered;
    auto ready = entered.get_future();
    std::thread writer([&] {
        std::unique_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
        entered.set_value();
    });
    const auto status = ready.wait_for(50ms);
    first.unlock();
    writer.join();
    EXPECT_EQ(std::future_status::timeout, status);
    std::shared_lock<OdbcDiagnosticsMutex> probe(diagnostics_mutex_, std::try_to_lock);
    EXPECT_TRUE(probe.owns_lock());
}

TEST_F(OdbcDiagnosticsMutexTest, ExclusiveCleanupProgressesUnderContinuousReaders) {
    constexpr int kObservers = 15;
    std::atomic<bool> stop{false};
    std::atomic<int> active{0};
    std::atomic<int> started{0};
    std::atomic<int> overlaps{0};
    std::atomic<bool> writing{false};
    std::vector<std::thread> readers;
    for (int i = 0; i < kObservers; ++i) {
        readers.emplace_back([&] {
            {
                std::shared_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
                started.fetch_add(1);
            }
            while (!stop.load()) {
                std::shared_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
                active.fetch_add(1);
                if (writing.load()) {
                    overlaps.fetch_add(1);
                }
                std::this_thread::yield();
                active.fetch_sub(1);
            }
        });
    }
    const auto deadline = std::chrono::steady_clock::now() + 3s;
    while (started.load() != kObservers && std::chrono::steady_clock::now() < deadline) {
        std::this_thread::yield();
    }
    EXPECT_EQ(kObservers, started.load());
    const auto before = std::chrono::steady_clock::now();
    for (int iteration = 0; iteration < 50; ++iteration) {
        std::unique_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
        writing.store(true);
        EXPECT_EQ(0, active.load());
        std::this_thread::yield();
        writing.store(false);
    }
    const auto elapsed = std::chrono::steady_clock::now() - before;
    stop.store(true);
    for (auto& reader : readers) {
        reader.join();
    }
    EXPECT_EQ(0, overlaps.load());
    EXPECT_LT(elapsed, 3s);
}

} // namespace
