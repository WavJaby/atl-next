// SPDX-License-Identifier: MIT
// Resolver check shared by guest fixtures (qemu-thread fixture.c check r, qemu-linker main_exe.c check n): getaddrinfo / getnameinfo / gethostbyname / gethostbyaddr on localhost through the host side, every field read in the guest's own bionic layout (LP64 or ILP32). No network: /etc/hosts and numeric forms only.
#ifndef ATL_TESTDATA_RESOLVER_H
#define ATL_TESTDATA_RESOLVER_H

#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>

// the service asked for, and its port
#define RS_SERVICE "80"
#define RS_PORT 80
// 127.0.0.1 in network order, byte by byte
static const unsigned char rs_loopback[4] = {127, 0, 0, 1};

static int rs_streq(const char *a, const char *b) {
    if (!a || !b) {
        return 0;
    }
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

static int rs_is_loopback(const void *p) {
    const unsigned char *b = p;
    return b[0] == rs_loopback[0] && b[1] == rs_loopback[1] && b[2] == rs_loopback[2] && b[3] == rs_loopback[3];
}

// Returns 0 when every field matched, else a bit per failed step: 1 getaddrinfo rc, 2 family / socktype / protocol, 3 addrlen, 4 sockaddr (port, address), 5 canonname, 6 getnameinfo rc, 7 its host / service text, 8 gethostbyname fields, 9 gethostbyaddr(127.0.0.1) fields, 14 the first node's ai_flags is not the request's (bionic values).
static long rs_resolve(void) {
    long bad = 0;
    struct addrinfo hints = {0};
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    // AI_NUMERICSERV: bionic 0x8, glibc 0x400 (bionic's AI_ADDRCONFIG), so a node carrying glibc's flag values fails bit 14
    hints.ai_flags = AI_CANONNAME | AI_NUMERICSERV;
    struct addrinfo *res = 0;
    if (getaddrinfo("localhost", RS_SERVICE, &hints, &res) != 0 || !res) {
        return 1L << 1;
    }
    // bionic getaddrinfo.c get_ai copies the request into each result node
    if (res->ai_flags != hints.ai_flags) {
        bad |= 1L << 14;
    }
    if (res->ai_family != AF_INET || res->ai_socktype != SOCK_STREAM || res->ai_protocol != IPPROTO_TCP) {
        bad |= 1L << 2;
    }
    if (res->ai_addrlen != sizeof(struct sockaddr_in)) {
        bad |= 1L << 3;
    }
    const struct sockaddr_in *sin = (const struct sockaddr_in *)res->ai_addr;
    if (!sin || sin->sin_family != AF_INET || ntohs(sin->sin_port) != RS_PORT || !rs_is_loopback(&sin->sin_addr)) {
        bad |= 1L << 4;
    }
    if (!rs_streq(res->ai_canonname, "localhost")) {
        bad |= 1L << 5;
    }
    char host[NI_MAXHOST];
    char serv[NI_MAXSERV];
    if (sin && !(bad & (1L << 4))) {
        if (getnameinfo(res->ai_addr, res->ai_addrlen, host, sizeof host, serv, sizeof serv, NI_NUMERICHOST | NI_NUMERICSERV) != 0) {
            bad |= 1L << 6;
        } else if (!rs_streq(host, "127.0.0.1") || !rs_streq(serv, RS_SERVICE)) {
            bad |= 1L << 7;
        }
    }
    freeaddrinfo(res);
    struct hostent *he = gethostbyname("127.0.0.1");
    if (!he || he->h_addrtype != AF_INET || he->h_length != 4 || !he->h_addr_list || !he->h_addr_list[0] || !rs_is_loopback(he->h_addr_list[0])) {
        bad |= 1L << 8;
    }
    // reverse lookup of the loopback address: /etc/hosts names it (glibc's files source), in bionic's hostent layout
    he = gethostbyaddr(rs_loopback, sizeof rs_loopback, AF_INET);
    if (!he || he->h_addrtype != AF_INET || he->h_length != 4 || !he->h_name || !he->h_name[0] || !he->h_addr_list || !he->h_addr_list[0] || !rs_is_loopback(he->h_addr_list[0])) {
        bad |= 1L << 9;
    }
    return bad;
}

// Walks a numeric lookup with no socket type: glibc answers one node per socket type (stream, datagram, raw), each 127.0.0.1. Returns 0, or 1L << 10 (rc), 1L << 11 (fewer than RS_MIN_NODES nodes), 1L << 12 (a node's family or address).
#define RS_MIN_NODES 2
__attribute__((unused)) static long rs_walk(void) {
    struct addrinfo hints = {0};
    hints.ai_flags = AI_NUMERICHOST;
    hints.ai_family = AF_INET;
    struct addrinfo *res = 0;
    if (getaddrinfo("127.0.0.1", 0, &hints, &res) != 0 || !res) {
        return 1L << 10;
    }
    long bad = 0;
    int n = 0;
    for (struct addrinfo *p = res; p; p = p->ai_next) {
        const struct sockaddr_in *sin = (const struct sockaddr_in *)p->ai_addr;
        if (p->ai_family != AF_INET || !sin || !rs_is_loopback(&sin->sin_addr)) {
            bad |= 1L << 12;
        }
        n++;
    }
    if (n < RS_MIN_NODES) {
        bad |= 1L << 11;
    }
    freeaddrinfo(res);
    return bad;
}

#endif
