// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Server-free checks of odbc_concurrent_test.h's SharedDbc retry path. A real
// busy connection needs live contention, so these drive the same
// RunSharedDbcIteration with a fixture whose body fails the way a contended
// body does.

#include "odbc_concurrent_test.h"

namespace {

using odbc_concurrency::detail::RetainedFixtures;
using odbc_concurrency::detail::RunSharedDbcIteration;
using odbc_concurrency::detail::WorkerResult;

constexpr const char* kBusy =
    "[HY000] [Microsoft][ODBC Driver 18 for SQL Server]Connection is busy with results for "
    "another command";

// ASSERT returns only from the function it is in, as in ODBCTest::ExecDirect.
void FailBusyInHelper() { FAIL() << kBusy; }

enum class FirstFailure {
    kFatalBusyInHelper,
    kNonFatalBusy,
    kBareNonFatalOnBusyStatement,
    kNonFatalOther,
};

struct Counts {
    int attempts = 0;
    int continued = 0;
    bool statement_busy = false;
};

// The first attempt fails as given, then carries on as a contended body would
// and fails again on the statement that never ran. Later attempts pass.
template <FirstFailure kFailure>
class FailsFirstAttempt {
   public:
    inline static Counts counts;

    explicit FailsFirstAttempt(odbc_concurrency::Mode) {}
    void ConcurrentSetUp(SQLHENV, SQLHDBC, int, int) {}
    void ConcurrentBody() {
        if (counts.attempts++ > 0) {
            return;
        }
        switch (kFailure) {
            case FirstFailure::kFatalBusyInHelper:
                FailBusyInHelper();
                break;
            case FirstFailure::kNonFatalBusy:
                ADD_FAILURE() << kBusy;
                break;
            case FirstFailure::kBareNonFatalOnBusyStatement:
                // An EXPECT_EQ on a return code: only the statement says busy.
                counts.statement_busy = true;
                ADD_FAILURE() << "Expected equality of these values: 0 and rc, which is -1";
                break;
            case FirstFailure::kNonFatalOther:
                ADD_FAILURE() << "[22003] Numeric value out of range";
                return;
        }
        ++counts.continued;
        ADD_FAILURE() << "[HY010] Function sequence error";
    }
    void ConcurrentTearDown() {}
    bool ConcurrentStatementBusy() const { return counts.statement_busy; }
};

template <typename T>
bool RunOneIteration(WorkerResult& result,
                     std::chrono::steady_clock::time_point deadline =
                         std::chrono::steady_clock::now() + std::chrono::seconds(60)) {
    T::counts = {};
    RetainedFixtures<T> retained;
    return RunSharedDbcIteration<T>(SQL_NULL_HENV, SQL_NULL_HDBC, odbc_concurrency::kSharedDbc,
                                    0, 0, "SharedDbc worker=0 iteration=0", result, retained,
                                    deadline);
}

// Busy on every attempt, up to a bound so a missing deadline check fails the
// test instead of hanging it.
class AlwaysBusy {
   public:
    inline static Counts counts;

    explicit AlwaysBusy(odbc_concurrency::Mode) {}
    void ConcurrentSetUp(SQLHENV, SQLHDBC, int, int) {}
    void ConcurrentBody() {
        if (++counts.attempts < 50) {
            ADD_FAILURE() << kBusy;
        }
    }
    void ConcurrentTearDown() {}
    bool ConcurrentStatementBusy() const { return false; }
};

template <typename T>
void ExpectStoppedAndRetried() {
    WorkerResult result;
    EXPECT_TRUE(RunOneIteration<T>(result));
    EXPECT_EQ(2, T::counts.attempts);
    EXPECT_EQ(0, T::counts.continued) << "the attempt went on past the busy failure";
    EXPECT_EQ(1, result.conflicts);
}

}  // namespace

TEST(ConcurrentHarnessTest, BusyAssertionInAHelperEndsTheAttemptAndRetriesIt) {
    ExpectStoppedAndRetried<FailsFirstAttempt<FirstFailure::kFatalBusyInHelper>>();
}

TEST(ConcurrentHarnessTest, NonFatalBusyFailureEndsTheAttemptAndRetriesIt) {
    ExpectStoppedAndRetried<FailsFirstAttempt<FirstFailure::kNonFatalBusy>>();
}

TEST(ConcurrentHarnessTest, BareNonFatalFailureOnABusyStatementEndsTheAttemptAndRetriesIt) {
    ExpectStoppedAndRetried<FailsFirstAttempt<FirstFailure::kBareNonFatalOnBusyStatement>>();
}

TEST(ConcurrentHarnessTest, OtherFailureFailsTheIterationWithoutARetry) {
    using T = FailsFirstAttempt<FirstFailure::kNonFatalOther>;
    WorkerResult result;
    bool ok = true;
    EXPECT_NONFATAL_FAILURE(ok = RunOneIteration<T>(result), "[22003]");
    EXPECT_FALSE(ok);
    EXPECT_EQ(1, T::counts.attempts);
    EXPECT_EQ(0, result.conflicts);
}

TEST(ConcurrentHarnessTest, ABusyAttemptPastTheDeadlineFailsWithoutARetry) {
    WorkerResult result;
    bool ok = true;
    EXPECT_NONFATAL_FAILURE(
        ok = RunOneIteration<AlwaysBusy>(result, std::chrono::steady_clock::now()),
        "at the retry deadline");
    EXPECT_FALSE(ok);
    EXPECT_EQ(1, AlwaysBusy::counts.attempts);
    EXPECT_EQ(1, result.conflicts);
}

// A fixed short wait livelocked long bodies at 8 x 20 on Linux; the wait has
// to grow with the attempt and stay within its cap.
TEST(ConcurrentHarnessTest, ConflictBackoffGrowsToItsCap) {
    using odbc_concurrency::detail::ConflictBackoff;
    using odbc_concurrency::detail::kConflictBackoffBase;
    using odbc_concurrency::detail::kConflictBackoffCap;
    std::minstd_rand rng(1);
    for (int attempt : {-1, 0, 1, 4, 8, 16, 40, 1000000}) {
        SCOPED_TRACE(attempt);
        const int shift = attempt < 0 ? 0 : (attempt > 16 ? 16 : attempt);
        const std::chrono::microseconds ceiling = std::min<std::chrono::microseconds>(
            kConflictBackoffCap, kConflictBackoffBase * (1 << shift));
        std::chrono::microseconds longest{0};
        for (int i = 0; i < 2000; ++i) {
            const std::chrono::microseconds wait = ConflictBackoff(attempt, rng);
            ASSERT_GE(wait.count(), 0);
            ASSERT_LE(wait, ceiling);
            if (wait > longest) {
                longest = wait;
            }
        }
        EXPECT_GT(longest, ceiling / 2) << "waits should spread up to the ceiling";
    }
}
