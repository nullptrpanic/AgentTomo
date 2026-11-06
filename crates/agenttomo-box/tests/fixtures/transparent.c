#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <signal.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static void check(int result, const char *name) { if (result < 0) { perror(name); exit(91); } }
static int socket4(int type) {
    int fd = socket(AF_INET, type, 0); check(fd, "socket");
    struct timeval timeout = {.tv_sec = 3};
    check(setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), "timeout");
    return fd;
}
static struct sockaddr_in peer(const char *ip, int port) {
    struct sockaddr_in address = {.sin_family=AF_INET, .sin_port=htons(port)};
    check(inet_pton(AF_INET, ip, &address.sin_addr), "address"); return address;
}
static void connect_to(int fd, struct sockaddr_in *address) {
    while (connect(fd, (void*)address, sizeof(*address)) < 0) {
        if (errno == EISCONN) break;
        if (errno != EINTR) { perror("connect"); exit(92); }
    }
    struct sockaddr_in actual; socklen_t size=sizeof(actual);
    check(getpeername(fd, (void*)&actual, &size), "getpeername");
    if (actual.sin_addr.s_addr != address->sin_addr.s_addr || actual.sin_port != address->sin_port) exit(93);
}
static void exchange(int fd) {
    ssize_t count;
    do { count=send(fd,"x",1,MSG_NOSIGNAL); } while(count<0 && errno==EINTR);
    if(count!=1)exit(94);
    char byte;
    do { count=recv(fd,&byte,1,0); } while(count<0 && errno==EINTR);
    if(count!=1 || byte!='x')exit(95);
}
static volatile sig_atomic_t signals;
struct server { int fd; int udp; };
static void *local_server(void *argument) {
    struct server *server=argument;
    char byte;
    if(server->udp){
        struct sockaddr_in peer; socklen_t size=sizeof(peer);
        if(recvfrom(server->fd,&byte,1,0,(void*)&peer,&size)!=1)exit(112);
        if(sendto(server->fd,&byte,1,0,(void*)&peer,size)!=1)exit(113);
    } else {
        int fd=accept(server->fd,NULL,NULL);check(fd,"accept");
        if(recv(fd,&byte,1,0)!=1 || send(fd,&byte,1,MSG_NOSIGNAL)!=1)exit(114);
        close(fd);
    }
    return NULL;
}
static void signal_handler(int signal, siginfo_t *info, void *context) {
    (void)signal; (void)context;
    if(info->si_code!=SI_KERNEL) _exit(96);
    signals++;
    // Bound the signal storm: ptrace delivery stops can exceed the timer period
    // even without an injected query, starving all ordinary application work.
    if(signals==256){struct itimerval stopped={0};setitimer(ITIMER_REAL,&stopped,NULL);}
}
int main(int argc, char **argv) {
    if(argc!=3)return 90;
    int port=atoi(argv[2]);
    if(!strcmp(argv[1],"local")) {
        for(int udp=0;udp<2;udp++){
            struct server server={.fd=socket4(udp?SOCK_DGRAM:SOCK_STREAM),.udp=udp};
            struct sockaddr_in address=peer("127.0.0.1",0); socklen_t size=sizeof(address);
            check(bind(server.fd,(void*)&address,sizeof(address)),"bind local");
            check(getsockname(server.fd,(void*)&address,&size),"local address");
            if(!udp)check(listen(server.fd,1),"listen");
            pthread_t thread; if(pthread_create(&thread,NULL,local_server,&server))return 115;
            int client=socket4(udp?SOCK_DGRAM:SOCK_STREAM);
            connect_to(client,&address);exchange(client);
            pthread_join(thread,NULL);close(client);close(server.fd);
        }
        puts("LOCAL_OK");
    } else if(!strcmp(argv[1],"reuse")) {
        int a=socket4(SOCK_STREAM), b=socket4(SOCK_STREAM), yes=1;
        check(setsockopt(a,SOL_SOCKET,SO_REUSEADDR,&yes,sizeof(yes)),"reuse a");
        check(setsockopt(b,SOL_SOCKET,SO_REUSEADDR,&yes,sizeof(yes)),"reuse b");
        struct sockaddr_in source=peer("127.0.0.1",0); socklen_t size=sizeof(source);
        check(bind(a,(void*)&source,sizeof(source)),"bind a");
        check(getsockname(a,(void*)&source,&size),"source a");
        check(bind(b,(void*)&source,sizeof(source)),"same source b");
        struct sockaddr_in first=peer("127.0.0.2",port), second=peer("127.0.0.3",port);
        connect_to(a,&first); connect_to(b,&second);
        struct sockaddr_in actual; size=sizeof(actual);check(getsockname(b,(void*)&actual,&size),"source b");
        if(actual.sin_port!=source.sin_port)exit(97);
        exchange(a);exchange(b);close(a);close(b);puts("REUSE_OK");
    } else if(!strcmp(argv[1],"signals")) {
        sigset_t blocked, actual;
        sigemptyset(&blocked);sigaddset(&blocked,SIGUSR1);
        check(sigprocmask(SIG_BLOCK,&blocked,NULL),"block signal");
        struct sigaction action={.sa_sigaction=signal_handler,.sa_flags=SA_SIGINFO|SA_RESTART};
        sigemptyset(&action.sa_mask);check(sigaction(SIGALRM,&action,NULL),"sigaction");
        struct itimerval timer={.it_interval={.tv_usec=20},.it_value={.tv_usec=20}};
        check(setitimer(ITIMER_REAL,&timer,NULL),"timer");
        struct sockaddr_in destination=peer("127.0.0.1",port);
        for(int i=0;i<100;i++){
            int fd=socket4(SOCK_STREAM);connect_to(fd,&destination);exchange(fd);close(fd);
            check(sigprocmask(SIG_SETMASK,NULL,&actual),"read signal mask");
            for(int signal=1;signal<NSIG;signal++)
                if(sigismember(&actual,signal)!=sigismember(&blocked,signal))return 116;
        }
        timer=(struct itimerval){0};check(setitimer(ITIMER_REAL,&timer,NULL),"stop timer");
        if(signals==0)exit(98);
        puts("SIGNALS_OK");
    } else if(!strcmp(argv[1],"udpmax")) {
        int fd=socket4(SOCK_DGRAM);struct sockaddr_in destination=peer("127.0.0.1",port);
        char *data=malloc(65507), *received=malloc(65507);
        if(!data || !received)return 99;
        for(int i=0;i<65507;i++)data[i]=(char)i;
        if(sendto(fd,data,65507,0,(void*)&destination,sizeof(destination))!=65507)return 100;
        struct sockaddr_in actual; socklen_t size=sizeof(actual);
        if(recvfrom(fd,received,65507,0,(void*)&actual,&size)!=65507 || memcmp(data,received,65507))return 101;
        if(actual.sin_port!=destination.sin_port || actual.sin_addr.s_addr!=destination.sin_addr.s_addr)return 102;
        close(fd);free(data);free(received);puts("MAX_OK");
    } else if(!strcmp(argv[1],"udp6max")) {
        int fd=socket(AF_INET6,SOCK_DGRAM,0);check(fd,"socket6");
        struct timeval timeout={.tv_sec=3};check(setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"timeout6");
        struct sockaddr_in6 destination={.sin6_family=AF_INET6,.sin6_port=htons(port),.sin6_addr=IN6ADDR_LOOPBACK_INIT};
        char *data=malloc(65527), *received=malloc(65527);
        if(!data || !received)return 104;
        for(int i=0;i<65527;i++)data[i]=(char)i;
        if(sendto(fd,data,65527,0,(void*)&destination,sizeof(destination))!=65527)return 105;
        struct sockaddr_in6 actual; socklen_t size=sizeof(actual);
        if(recvfrom(fd,received,65527,0,(void*)&actual,&size)!=65527 || memcmp(data,received,65527))return 106;
        if(actual.sin6_port!=destination.sin6_port || memcmp(&actual.sin6_addr,&destination.sin6_addr,16))return 107;
        close(fd);free(data);free(received);puts("MAX6_OK");
    } else if(!strcmp(argv[1],"udp-shared-tuple")) {
        int a=socket4(SOCK_DGRAM), b=socket4(SOCK_DGRAM), yes=1;
        check(setsockopt(a,SOL_SOCKET,SO_REUSEADDR,&yes,sizeof(yes)),"reuse a");
        check(setsockopt(b,SOL_SOCKET,SO_REUSEADDR,&yes,sizeof(yes)),"reuse b");
        struct sockaddr_in source=peer("127.0.0.1",0), destination=peer("127.0.0.1",port);
        socklen_t size=sizeof(source);
        check(bind(a,(void*)&source,sizeof(source)),"bind a");
        check(getsockname(a,(void*)&source,&size),"source a");
        check(bind(b,(void*)&source,sizeof(source)),"bind b");
        pid_t child=fork();check(child,"fork");
        if(child==0){usleep(50000);if(sendto(b,"second",6,0,(void*)&destination,sizeof(destination))!=6)return 110;}
        else if(sendto(a,"first",5,0,(void*)&destination,sizeof(destination))!=5)return 111;
        close(a);close(b);
    } else if(!strcmp(argv[1],"udp-exit")) {
        int fd=socket4(SOCK_DGRAM);struct sockaddr_in destination=peer("127.0.0.1",port);
        if(sendto(fd,"queued",6,0,(void*)&destination,sizeof(destination))!=6)return 108;
        if(sendto(fd,"",0,0,(void*)&destination,sizeof(destination))!=0)return 109;
        close(fd);
    } else return 103;
    return 0;
}
