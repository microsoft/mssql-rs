// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#pragma once

#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
#else
#include <netdb.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

#include <atomic>
#include <condition_variable>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>

// Relays TLS bytes unchanged. Pausing only the client-to-server direction lets
// cancellation tests hold a packet write pending without stopping SQL Server.
class TcpPauseProxy {
#ifdef _WIN32
    using Socket = SOCKET;
    static constexpr Socket Invalid = INVALID_SOCKET;
    static constexpr int Both = SD_BOTH;
    struct SocketRuntime {
        SocketRuntime() {
            WSADATA data;
            if (WSAStartup(MAKEWORD(2, 2), &data) != 0)
                throw std::runtime_error("WSAStartup failed");
        }
        ~SocketRuntime() { WSACleanup(); }
    };
#else
    using Socket = int;
    static constexpr Socket Invalid = -1;
    static constexpr int Both = SHUT_RDWR;
    struct SocketRuntime {};
#endif
    static void Close(Socket socket) {
        if (socket == Invalid) return;
#ifdef _WIN32
        closesocket(socket);
#else
        close(socket);
#endif
    }
    struct OwnedSocket {
        Socket value = Invalid;
        ~OwnedSocket() { Close(value); }
        void Reset(Socket socket = Invalid) {
            Close(value);
            value = socket;
        }
    };

public:
    explicit TcpPauseProxy(std::string server) {
        if (server.rfind("tcp:", 0) == 0) server.erase(0, 4);
        std::string port = "1433";
        const auto comma = server.rfind(',');
        if (comma != std::string::npos) {
            port = server.substr(comma + 1);
            server.resize(comma);
        }
        if (server.size() > 1 && server.front() == '[' && server.back() == ']')
            server = server.substr(1, server.size() - 2);
        addrinfo hints{};
        hints.ai_socktype = SOCK_STREAM;
        hints.ai_family = AF_UNSPEC;
        addrinfo* addresses = nullptr;
        if (getaddrinfo(server.c_str(), port.c_str(), &hints, &addresses) != 0)
            throw std::runtime_error("Cannot resolve proxy SQL Server endpoint");
        for (auto* address = addresses; address; address = address->ai_next) {
            remote_.Reset(socket(address->ai_family, SOCK_STREAM, IPPROTO_TCP));
            if (remote_.value != Invalid &&
                connect(remote_.value, address->ai_addr,
                        static_cast<int>(address->ai_addrlen)) == 0)
                break;
            remote_.Reset();
        }
        freeaddrinfo(addresses);
        if (remote_.value == Invalid)
            throw std::runtime_error("Cannot connect proxy to SQL Server");

        listener_.Reset(socket(AF_INET, SOCK_STREAM, IPPROTO_TCP));
        if (listener_.value == Invalid)
            throw std::runtime_error("Cannot create proxy listener");
        // Set before the handshake so the advertised receive window is small.
        const int receiveBuffer = 4096;
        if (setsockopt(listener_.value, SOL_SOCKET, SO_RCVBUF,
                       reinterpret_cast<const char*>(&receiveBuffer),
                       sizeof(receiveBuffer)) != 0)
            throw std::runtime_error("Cannot bound proxy receive window");
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (bind(listener_.value, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0 ||
            listen(listener_.value, 1) != 0)
            throw std::runtime_error("Cannot bind proxy listener");
#ifdef _WIN32
        int length = sizeof(address);
#else
        socklen_t length = sizeof(address);
#endif
        if (getsockname(listener_.value, reinterpret_cast<sockaddr*>(&address), &length) != 0)
            throw std::runtime_error("Cannot get proxy port");
        port_ = ntohs(address.sin_port);
        const auto listener = listener_.value;
        worker_ = std::thread([this, listener] {
            const auto client = accept(listener, nullptr, nullptr);
            client_.store(client);
            if (client == Invalid || stopped_.load()) return;
            std::thread responses([&] { Relay(remote_.value, client, false); });
            Relay(client, remote_.value, true);
            shutdown(remote_.value, Both);
            shutdown(client, Both);
            responses.join();
        });
    }

    ~TcpPauseProxy() {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            stopped_.store(true);
            paused_ = false;
        }
        resumed_.notify_all();
        shutdown(listener_.value, Both);
        listener_.Reset();
        shutdown(remote_.value, Both);
        const auto client = client_.load();
        if (client != Invalid) shutdown(client, Both);
        if (worker_.joinable()) worker_.join();
        Close(client_.load());
    }

    unsigned Port() const { return port_; }
    bool Failed() const { return failed_.load(); }

    void Pause() {
        std::lock_guard<std::mutex> lock(mutex_);
        paused_ = true;
    }

    void Resume() {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            paused_ = false;
        }
        resumed_.notify_all();
    }

private:
    void Relay(Socket from, Socket to, bool request) {
#ifdef SO_NOSIGPIPE
        const int enabled = 1;
        if (setsockopt(to, SOL_SOCKET, SO_NOSIGPIPE,
                       reinterpret_cast<const char*>(&enabled), sizeof(enabled)) != 0) {
            failed_.store(true);
            return;
        }
#endif
        char buffer[4096];
        while (!stopped_.load()) {
            if (request) {
                std::unique_lock<std::mutex> lock(mutex_);
                resumed_.wait(lock, [&] { return !paused_ || stopped_.load(); });
                if (stopped_.load()) return;
            }
            const auto received = recv(from, buffer, sizeof(buffer), 0);
            if (received <= 0) {
                if (received < 0 && !stopped_.load()) failed_.store(true);
                return;
            }
            int offset = 0;
            while (offset < received) {
#ifdef MSG_NOSIGNAL
                constexpr int flags = MSG_NOSIGNAL;
#else
                constexpr int flags = 0;
#endif
                const auto sent = send(to, buffer + offset,
                                       static_cast<int>(received) - offset, flags);
                if (sent <= 0) {
                    if (!stopped_.load()) failed_.store(true);
                    return;
                }
                offset += static_cast<int>(sent);
            }
        }
    }

    SocketRuntime runtime_;
    OwnedSocket listener_;
    OwnedSocket remote_;
    std::atomic<Socket> client_{Invalid};
    std::atomic<bool> stopped_{false};
    std::atomic<bool> failed_{false};
    std::mutex mutex_;
    std::condition_variable resumed_;
    bool paused_ = false;
    unsigned port_ = 0;
    std::thread worker_;
};
