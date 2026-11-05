// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_bench.hpp"

#include <algorithm>
#include <cctype>
#include <iomanip>
#include <iostream>
#include <locale>
#include <memory>
#include <sstream>
#include <stdexcept>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace mssql::odbc::bench {
namespace {

constexpr std::size_t kDefaultPairedRounds = 100;
// Below ten rounds a one-sided 1% sign test cannot be met by any outcome
// short of unanimity, so the comparator's gate rule would be meaningless.
constexpr std::size_t kMinPairedRounds = 10;
constexpr std::size_t kMaxPairedRounds = 1000;
constexpr std::size_t kMaxPairedCalls = 1'000'000;
constexpr std::size_t kMaxCandidateDelayNs = 1'000'000;
constexpr char kPairedFormat[] = "mssql-odbc-bench-paired/1";

std::size_t bounded_environment_integer(const char* name, std::size_t fallback,
                                        std::size_t minimum, std::size_t maximum) {
    const auto raw = environment_value(name);
    if (raw.empty()) {
        return fallback;
    }
    const bool digits = raw.size() <= 9 &&
                        std::all_of(raw.begin(), raw.end(), [](unsigned char character) {
                            return std::isdigit(character) != 0;
                        });
    const std::size_t value = digits ? static_cast<std::size_t>(std::stoul(raw)) : 0;
    if (!digits || value < minimum || value > maximum) {
        std::ostringstream message;
        message << name << " must be an integer between " << minimum << " and " << maximum;
        throw std::runtime_error(message.str());
    }
    return value;
}

void append_json_string(std::ostream& output, std::string_view value) {
    output << '"';
    for (const char character : value) {
        const auto byte = static_cast<unsigned char>(character);
        if (character == '"' || character == '\\') {
            output << '\\' << character;
        } else if (byte < 0x20) {
            output << "\\u" << std::hex << std::setw(4) << std::setfill('0')
                   << static_cast<int>(byte) << std::dec << std::setfill(' ');
        } else {
            output << character;
        }
    }
    output << '"';
}

double median(std::vector<double> values) {
    std::sort(values.begin(), values.end());
    const auto middle = values.size() / 2;
    return values.size() % 2 == 1 ? values[middle]
                                  : (values[middle - 1] + values[middle]) / 2.0;
}

}  // namespace

PairedConfig PairedConfig::from_environment() {
    const auto candidate = Config::from_environment();
    PairedConfig config;
    config.legs.push_back({"candidate", candidate});

    auto baseline = candidate;
    baseline.driver = environment_value("ODBC_BENCH_PAIRED_BASELINE_DRIVER");
    if (baseline.driver.empty()) {
        throw std::runtime_error("missing required ODBC_BENCH_PAIRED_BASELINE_DRIVER");
    }
    config.legs.push_back({"baseline", baseline});

    const auto reference_driver = environment_value("ODBC_BENCH_PAIRED_REFERENCE_DRIVER");
    if (!reference_driver.empty()) {
        auto reference = candidate;
        reference.driver = reference_driver;
        const auto keyword =
            environment_value("ODBC_BENCH_PAIRED_REFERENCE_PACKET_SIZE_KEYWORD");
        if (!keyword.empty()) {
            if (keyword != "PacketSize" && keyword != "Packet Size") {
                throw std::runtime_error(
                    "ODBC_BENCH_PAIRED_REFERENCE_PACKET_SIZE_KEYWORD must be PacketSize "
                    "or Packet Size");
            }
            reference.packet_size_keyword = keyword;
        }
        config.legs.push_back({"reference", reference});
    }

    config.rounds = bounded_environment_integer("ODBC_BENCH_PAIRED_ROUNDS",
                                                kDefaultPairedRounds, kMinPairedRounds,
                                                kMaxPairedRounds);
    config.calls_override =
        bounded_environment_integer("ODBC_BENCH_PAIRED_CALLS", 0, 1, kMaxPairedCalls);
    config.candidate_delay = std::chrono::nanoseconds(bounded_environment_integer(
        "ODBC_BENCH_PAIRED_CANDIDATE_DELAY_NS", 0, 0, kMaxCandidateDelayNs));
    return config;
}

std::string run_paired_latency(const PairedConfig& config) {
    if (config.legs.size() < 2 || config.legs[0].role != "candidate" ||
        config.legs[1].role != "baseline") {
        throw std::logic_error("paired latency needs candidate and baseline legs first");
    }
    if (config.candidate_delay.count() != 0) {
        std::cerr << "WARNING: harness self-test: the candidate leg is slowed by "
                  << config.candidate_delay.count()
                  << " ns per call. Never set ODBC_BENCH_PAIRED_CANDIDATE_DELAY_NS "
                     "in a real comparison.\n";
    }

    // Every leg keeps its own environment and connection for the whole run, so
    // a round switches drivers without reconnecting.
    std::vector<std::unique_ptr<OdbcSession>> sessions;
    sessions.reserve(config.legs.size());
    for (const auto& leg : config.legs) {
        sessions.push_back(std::make_unique<OdbcSession>(leg.config));
    }

    // Candidate and baseline swap places every round (ABBA), so they share the
    // same mean position and linear drift cancels in their ratio. A reference
    // leg always runs last: it separates rounds, so no leg's batch directly
    // follows its own and inherits caches the others do not get.
    std::vector<std::size_t> even_order(config.legs.size());
    for (std::size_t index = 0; index < even_order.size(); ++index) {
        even_order[index] = index;
    }
    auto odd_order = even_order;
    std::swap(odd_order[0], odd_order[1]);

    std::ostringstream json;
    json.imbue(std::locale::classic());
    json << std::setprecision(12);
    json << "{\n  \"format\": ";
    append_json_string(json, kPairedFormat);
    json << ",\n  \"rounds\": " << config.rounds
         << ",\n  \"candidate_delay_ns\": " << config.candidate_delay.count()
         << ",\n  \"legs\": [";
    for (std::size_t index = 0; index < config.legs.size(); ++index) {
        json << (index == 0 ? "\n" : ",\n") << "    {\"role\": ";
        append_json_string(json, config.legs[index].role);
        json << ", \"driver\": ";
        append_json_string(json, config.legs[index].config.driver);
        json << '}';
    }
    json << "\n  ],\n  \"benchmarks\": [";

    bool first_benchmark = true;
    for (const auto& spec : latency_workloads()) {
        const std::size_t calls =
            config.calls_override != 0 ? config.calls_override : spec.paired_calls_per_batch;
        // A short untimed run on the same leg right before each timed batch, so
        // no batch pays for the caches the previous leg's driver evicted.
        const std::size_t warm_calls = std::max<std::size_t>(20, calls / 10);

        std::vector<std::unique_ptr<LatencyRunner>> runners;
        runners.reserve(config.legs.size());
        for (std::size_t index = 0; index < config.legs.size(); ++index) {
            auto runner = std::make_unique<LatencyRunner>(*sessions[index], spec, calls);
            if (config.legs[index].role == "candidate") {
                runner->set_per_call_delay(config.candidate_delay);
            }
            std::cerr << '[' << config.legs[index].role << "] ";
            runner->preflight();
            runners.push_back(std::move(runner));
        }

        std::vector<std::vector<double>> seconds(
            config.legs.size(), std::vector<double>(config.rounds, 0.0));
        for (std::size_t round = 0; round < config.rounds; ++round) {
            for (const auto index : round % 2 == 0 ? even_order : odd_order) {
                runners[index]->warm_up(warm_calls);
                seconds[index][round] = runners[index]->execute().total_seconds;
            }
        }

        std::vector<double> ratios(config.rounds);
        for (std::size_t round = 0; round < config.rounds; ++round) {
            ratios[round] = seconds[0][round] / seconds[1][round];
        }
        std::cerr << spec.paired_name << ": median candidate/baseline ratio "
                  << median(ratios) << " over " << config.rounds << " rounds of " << calls
                  << " calls\n";

        json << (first_benchmark ? "\n" : ",\n") << "    {\"name\": ";
        first_benchmark = false;
        append_json_string(json, spec.paired_name);
        json << ", \"calls_per_batch\": " << calls
             << ", \"warm_up_calls_per_batch\": " << warm_calls << ", \"seconds\": {";
        for (std::size_t index = 0; index < config.legs.size(); ++index) {
            json << (index == 0 ? "" : ", ");
            append_json_string(json, config.legs[index].role);
            json << ": [";
            for (std::size_t round = 0; round < config.rounds; ++round) {
                json << (round == 0 ? "" : ", ") << seconds[index][round];
            }
            json << ']';
        }
        json << "}}";
    }
    json << "\n  ]\n}\n";
    return json.str();
}

}  // namespace mssql::odbc::bench
